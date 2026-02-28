//! Configuration types.
//!
//! All types implement [`Default`] for compile-time fallback values.

use orcs_hook::HooksConfig;
use orcs_mcp::McpConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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

    /// Component loading configuration.
    pub components: ComponentsConfig,

    /// Hooks configuration.
    pub hooks: HooksConfig,

    /// MCP (Model Context Protocol) server configuration.
    pub mcp: McpConfig,

    /// Timeout configuration.
    pub timeouts: TimeoutsConfig,
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

    /// Returns global config fields as a JSON value for Lua component injection.
    ///
    /// This is injected into each component's `init(cfg)` table under the `_global` key
    /// by `OrcsAppBuilder::build()`, so Lua components can access it as `cfg._global`.
    ///
    /// # Included fields
    ///
    /// | Key | Type | Description |
    /// |-----|------|-------------|
    /// | `debug` | bool | Debug mode flag |
    /// | `model.default` | string | Default model name |
    /// | `model.temperature` | number | Default sampling temperature |
    /// | `model.max_tokens` | number\|null | Default max completion tokens |
    /// | `hil.auto_approve` | bool | Auto-approve requests |
    /// | `hil.timeout_ms` | number | Approval timeout (ms) |
    /// | `ui.verbose` | bool | Verbose output |
    /// | `ui.color` | bool | Color output |
    /// | `ui.emoji` | bool | Emoji output |
    /// | `timeouts.delegate_ms` | number | Delegate task timeout (ms) |
    ///
    /// # Excluded fields
    ///
    /// `paths`, `scripts`, `components`, `hooks` are excluded (internal/sensitive).
    #[must_use]
    pub fn global_config_for_lua(&self) -> serde_json::Value {
        serde_json::json!({
            "debug": self.debug,
            "model": {
                "default": self.model.default,
                "temperature": self.model.temperature,
                "max_tokens": self.model.max_tokens,
            },
            "hil": {
                "auto_approve": self.hil.auto_approve,
                "timeout_ms": self.hil.timeout_ms,
            },
            "ui": {
                "verbose": self.ui.verbose,
                "color": self.ui.color,
                "emoji": self.ui.emoji,
            },
            "timeouts": {
                "delegate_ms": self.timeouts.delegate_ms,
            },
        })
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
        self.components.merge(&other.components);
        self.hooks.merge(&other.hooks);
        self.mcp.merge(&other.mcp);
        self.timeouts.merge(&other.timeouts);
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
            default: "llama3.2:latest".into(),
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

    /// Shell history file path.
    ///
    /// Set by `--sandbox` to redirect history to sandbox dir.
    /// When `None`, defaults to `~/.orcs/history`.
    pub history_file: Option<PathBuf>,
}

impl PathsConfig {
    fn merge(&mut self, other: &Self) {
        if other.session_dir.is_some() {
            self.session_dir = other.session_dir.clone();
        }
        if other.cache_dir.is_some() {
            self.cache_dir = other.cache_dir.clone();
        }
        if other.history_file.is_some() {
            self.history_file = other.history_file.clone();
        }
    }

    /// Returns the session directory, falling back to default.
    #[must_use]
    pub fn session_dir_or_default(&self) -> PathBuf {
        self.session_dir
            .clone()
            .unwrap_or_else(crate::session::default_session_path)
    }

    /// Returns the history file path, falling back to `~/.orcs/history`.
    #[must_use]
    pub fn history_file_or_default(&self) -> PathBuf {
        self.history_file.clone().unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".orcs")
                .join("history")
        })
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

/// Timeout configuration for Lua component operations.
///
/// These values are injected into `cfg._global.timeouts` for Lua access.
///
/// # Example TOML
///
/// ```toml
/// [timeouts]
/// delegate_ms = 600000
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TimeoutsConfig {
    /// Timeout for delegate/invoke operations in milliseconds.
    ///
    /// Used by agent_mgr, concierge, and delegate_worker for task
    /// delegation. Default: 600000 (10 minutes).
    pub delegate_ms: u64,
}

impl Default for TimeoutsConfig {
    fn default() -> Self {
        Self {
            delegate_ms: 600_000,
        }
    }
}

impl TimeoutsConfig {
    fn merge(&mut self, other: &Self) {
        let default = Self::default();

        if other.delegate_ms != default.delegate_ms {
            self.delegate_ms = other.delegate_ms;
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

/// Component loading configuration.
///
/// Controls which components are loaded at startup and where to find them.
///
/// # Example TOML
///
/// ```toml
/// [components]
/// load = ["agent_mgr", "skill_manager", "profile_manager", "shell", "tool"]
/// experimental = ["life_game"]
/// paths = ["~/.orcs/components"]
/// builtins_dir = "~/.orcs/builtins"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ComponentsConfig {
    /// Component names to load at startup.
    ///
    /// Each name is resolved via `ScriptLoader` from `paths` (user-first)
    /// then from the versioned builtins directory.
    pub load: Vec<String>,

    /// Experimental component names.
    ///
    /// These are only loaded when the `--experimental` CLI flag or
    /// `ORCS_EXPERIMENTAL=true` environment variable is set.
    /// They are appended to `load` at config resolution time.
    pub experimental: Vec<String>,

    /// User component search directories (priority order, searched first).
    ///
    /// Supports `~` expansion.
    pub paths: Vec<PathBuf>,

    /// Directory for builtin scripts (expanded from embedded on first run).
    ///
    /// Supports `~` expansion. Defaults to `~/.orcs/builtins`.
    pub builtins_dir: PathBuf,

    /// Per-component settings, keyed by component name.
    ///
    /// Values are passed to `Component::init(config)` at startup as a Lua table.
    /// Each key maps to an arbitrary JSON object (no schema enforcement on the
    /// Rust side — each Lua component validates its own keys).
    ///
    /// # Data Flow
    ///
    /// ```text
    /// config.toml [components.settings.<name>]
    ///   → serde → HashMap<String, serde_json::Value>
    ///   → component_settings(name) → serde_json::Value
    ///   → builder.rs injects cfg._global (OrcsConfig::global_config_for_lua())
    ///   → ChannelRunner.with_component_config()
    ///   → LuaComponent::init(json) → lua.to_value(json)
    ///   → Lua: init(cfg)
    /// ```
    ///
    /// # Global Config (`cfg._global`)
    ///
    /// Every component receives global config under `cfg._global`, injected
    /// by `OrcsAppBuilder::build()`. See [`OrcsConfig::global_config_for_lua()`]
    /// for the full field list. Components access it as:
    ///
    /// ```lua
    /// init = function(cfg)
    ///     local debug = cfg._global.debug
    ///     local model = cfg._global.model.default
    /// end
    /// ```
    ///
    /// # Known Component Settings
    ///
    /// ## `agent_mgr`
    ///
    /// | Key | Type | Default | Description |
    /// |-----|------|---------|-------------|
    /// | `prompt_placement` | `"top"\|"both"\|"bottom"` | `"both"` | Where skills/tools appear in the prompt |
    /// | `llm_provider` | `"ollama"\|"openai"\|"anthropic"` | `"ollama"` | LLM provider for agent conversation |
    /// | `llm_model` | string | provider default | Model name |
    /// | `llm_base_url` | string | provider default | Provider base URL |
    /// | `llm_api_key` | string | env var fallback | API key |
    /// | `llm_temperature` | number | none | Sampling temperature |
    /// | `llm_max_tokens` | number | none | Max completion tokens |
    /// | `llm_timeout` | number | `120` | Request timeout (seconds) |
    /// | `ping_timeout_ms` | number | `5000` | External agent ping timeout (ms) |
    ///
    /// ## `lua_sandbox`
    ///
    /// | Key | Type | Default | Description |
    /// |-----|------|---------|-------------|
    /// | `sandbox_timeout_ms` | number | `30000` | Sandbox execution timeout (ms) |
    ///
    /// ## `skill_manager`
    ///
    /// | Key | Type | Default | Description |
    /// |-----|------|---------|-------------|
    /// | `recommend_skill` | bool | `true` | Enable LLM-based skill recommendation (false = keyword fallback) |
    /// | `recommend_llm_provider` | `"ollama"\|"openai"\|"anthropic"` | `"ollama"` | LLM provider for recommend |
    /// | `recommend_llm_model` | string | provider default | Model name for recommend |
    /// | `recommend_llm_base_url` | string | provider default | Provider base URL for recommend |
    /// | `recommend_llm_api_key` | string | env var fallback | API key for recommend |
    /// | `recommend_llm_temperature` | number | none | Sampling temperature for recommend |
    /// | `recommend_llm_max_tokens` | number | none | Max tokens for recommend |
    /// | `recommend_llm_timeout` | number | `120` | Timeout for recommend (seconds) |
    ///
    /// # Example (TOML)
    ///
    /// ```toml
    /// [components.settings.agent_mgr]
    /// prompt_placement = "both"
    /// llm_provider = "ollama"
    /// llm_model    = "qwen2.5-coder:1.5b"
    ///
    /// [components.settings.skill_manager]
    /// recommend_skill      = true
    /// recommend_llm_provider = "ollama"
    /// recommend_llm_model    = "qwen2.5-coder:1.5b"
    /// ```
    #[serde(default)]
    pub settings: HashMap<String, serde_json::Value>,
}

impl Default for ComponentsConfig {
    fn default() -> Self {
        Self {
            load: vec![
                "agent_mgr".into(),
                "skill_manager".into(),
                "mcp_manager".into(),
                "profile_manager".into(),
                "foundation_manager".into(),
                "console_metrics".into(),
                "shell".into(),
                "tool".into(),
            ],
            experimental: vec!["life_game".into()],
            paths: vec![PathBuf::from("~/.orcs/components")],
            builtins_dir: PathBuf::from("~/.orcs/builtins"),
            settings: HashMap::new(),
        }
    }
}

impl ComponentsConfig {
    /// Resolves builtins_dir with tilde expansion.
    #[must_use]
    pub fn resolved_builtins_dir(&self) -> PathBuf {
        expand_tilde(&self.builtins_dir)
    }

    /// Resolves user component paths with tilde expansion.
    ///
    /// Non-existent directories are **included** (they may be created later).
    #[must_use]
    pub fn resolved_paths(&self) -> Vec<PathBuf> {
        self.paths.iter().map(|p| expand_tilde(p)).collect()
    }

    /// Appends experimental component names to `load`, deduplicating.
    ///
    /// Called by config resolvers when `--experimental` or `ORCS_EXPERIMENTAL=true` is set.
    pub fn activate_experimental(&mut self) {
        for name in &self.experimental {
            if !self.load.contains(name) {
                self.load.push(name.clone());
            }
        }
    }

    /// Returns the per-component settings for `name`, or an empty JSON object.
    ///
    /// This returns only `[components.settings.<name>]` from config.toml.
    /// Global config (`_global`) is injected separately by the builder
    /// (see [`OrcsConfig::global_config_for_lua()`]).
    #[must_use]
    pub fn component_settings(&self, name: &str) -> serde_json::Value {
        self.settings
            .get(name)
            .cloned()
            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()))
    }

    fn merge(&mut self, other: &Self) {
        let default = Self::default();

        if other.load != default.load {
            self.load = other.load.clone();
        }
        if other.experimental != default.experimental {
            self.experimental = other.experimental.clone();
        }
        if other.paths != default.paths {
            self.paths = other.paths.clone();
        }
        if other.builtins_dir != default.builtins_dir {
            self.builtins_dir = other.builtins_dir.clone();
        }
        // Settings: merge per-component (other overwrites keys present in self)
        for (key, value) in &other.settings {
            self.settings.insert(key.clone(), value.clone());
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
        assert_eq!(config.model.default, "llama3.2:latest");
        assert!(!config.hil.auto_approve);
        assert!(config.hooks.hooks.is_empty());
    }

    #[test]
    fn toml_roundtrip() {
        let config = OrcsConfig::default();
        let toml = config
            .to_toml()
            .expect("should serialize default config to TOML");
        let restored = OrcsConfig::from_toml(&toml).expect("should deserialize roundtripped TOML");
        assert_eq!(config, restored);
    }

    #[test]
    fn toml_partial_parse() {
        let toml = r#"
debug = true

[model]
default = "custom-model"
"#;
        let config = OrcsConfig::from_toml(toml).expect("should parse partial TOML with defaults");
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
        let config = OrcsConfig::from_toml(toml).expect("should parse scripts config from TOML");
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

    // === HooksConfig integration tests ===

    #[test]
    fn hooks_config_toml_parse() {
        let toml = r#"
[[hooks.hooks]]
id = "audit"
fql = "builtin::*"
point = "request.pre_dispatch"
script = "hooks/audit.lua"
priority = 50
enabled = true
"#;
        let config = OrcsConfig::from_toml(toml).expect("should parse hooks config from TOML");
        assert_eq!(config.hooks.hooks.len(), 1);
        assert_eq!(config.hooks.hooks[0].id.as_deref(), Some("audit"));
        assert_eq!(config.hooks.hooks[0].priority, 50);
    }

    #[test]
    fn hooks_config_toml_roundtrip() {
        let mut config = OrcsConfig::default();
        config.hooks = HooksConfig {
            hooks: vec![orcs_hook::HookDef {
                id: Some("test-hook".into()),
                fql: "*::*".into(),
                point: "request.pre_dispatch".into(),
                script: Some("hooks/test.lua".into()),
                handler_inline: None,
                priority: 100,
                enabled: true,
            }],
        };

        let toml = config
            .to_toml()
            .expect("should serialize config with hooks to TOML");
        let restored =
            OrcsConfig::from_toml(&toml).expect("should deserialize hooks roundtrip TOML");
        assert_eq!(config.hooks, restored.hooks);
    }

    #[test]
    fn hooks_config_merge() {
        let mut base = OrcsConfig::default();
        base.hooks = HooksConfig {
            hooks: vec![orcs_hook::HookDef {
                id: Some("h1".into()),
                fql: "*::*".into(),
                point: "request.pre_dispatch".into(),
                script: Some("base.lua".into()),
                handler_inline: None,
                priority: 100,
                enabled: true,
            }],
        };

        let overlay = OrcsConfig {
            hooks: HooksConfig {
                hooks: vec![orcs_hook::HookDef {
                    id: Some("h2".into()),
                    fql: "builtin::llm".into(),
                    point: "tool.pre_execute".into(),
                    script: Some("overlay.lua".into()),
                    handler_inline: None,
                    priority: 50,
                    enabled: true,
                }],
            },
            ..Default::default()
        };

        base.merge(&overlay);
        assert_eq!(base.hooks.hooks.len(), 2);
        assert_eq!(base.hooks.hooks[0].id.as_deref(), Some("h1"));
        assert_eq!(base.hooks.hooks[1].id.as_deref(), Some("h2"));
    }

    #[test]
    fn hooks_config_merge_override_same_id() {
        let mut base = OrcsConfig::default();
        base.hooks = HooksConfig {
            hooks: vec![orcs_hook::HookDef {
                id: Some("h1".into()),
                fql: "*::*".into(),
                point: "request.pre_dispatch".into(),
                script: Some("old.lua".into()),
                handler_inline: None,
                priority: 100,
                enabled: true,
            }],
        };

        let overlay = OrcsConfig {
            hooks: HooksConfig {
                hooks: vec![orcs_hook::HookDef {
                    id: Some("h1".into()),
                    fql: "builtin::llm".into(),
                    point: "request.pre_dispatch".into(),
                    script: Some("new.lua".into()),
                    handler_inline: None,
                    priority: 10,
                    enabled: true,
                }],
            },
            ..Default::default()
        };

        base.merge(&overlay);
        assert_eq!(base.hooks.hooks.len(), 1);
        assert_eq!(base.hooks.hooks[0].script.as_deref(), Some("new.lua"));
        assert_eq!(base.hooks.hooks[0].priority, 10);
    }

    #[test]
    fn hooks_config_empty_merge_preserves_base() {
        let mut base = OrcsConfig::default();
        base.hooks = HooksConfig {
            hooks: vec![orcs_hook::HookDef {
                id: Some("h1".into()),
                fql: "*::*".into(),
                point: "request.pre_dispatch".into(),
                script: Some("keep.lua".into()),
                handler_inline: None,
                priority: 100,
                enabled: true,
            }],
        };

        let overlay = OrcsConfig::default();
        base.merge(&overlay);

        assert_eq!(base.hooks.hooks.len(), 1);
        assert_eq!(base.hooks.hooks[0].id.as_deref(), Some("h1"));
    }

    // === ComponentsConfig experimental tests ===

    #[test]
    fn components_default_has_experimental() {
        let config = ComponentsConfig::default();
        assert_eq!(config.experimental, vec!["life_game"]);
        assert!(!config.load.contains(&"life_game".to_string()));
    }

    #[test]
    fn activate_experimental_appends_to_load() {
        let mut config = ComponentsConfig::default();
        config.activate_experimental();

        assert!(config.load.contains(&"life_game".to_string()));
        // Original load items preserved
        assert!(config.load.contains(&"agent_mgr".to_string()));
    }

    #[test]
    fn activate_experimental_deduplicates() {
        let mut config = ComponentsConfig::default();
        // Manually add life_game to load first
        config.load.push("life_game".into());
        let len_before = config.load.len();

        config.activate_experimental();

        // No duplicate added
        assert_eq!(config.load.len(), len_before);
    }

    #[test]
    fn activate_experimental_custom_list() {
        let mut config = ComponentsConfig::default();
        config.experimental = vec!["foo".into(), "bar".into()];
        config.activate_experimental();

        assert!(config.load.contains(&"foo".to_string()));
        assert!(config.load.contains(&"bar".to_string()));
        // Default experimental not present (was overridden)
        assert!(!config.load.contains(&"life_game".to_string()));
    }

    #[test]
    fn components_experimental_toml_roundtrip() {
        let config = ComponentsConfig::default();
        let orcs = OrcsConfig {
            components: config,
            ..Default::default()
        };
        let toml = orcs.to_toml().expect("should serialize");
        let restored = OrcsConfig::from_toml(&toml).expect("should deserialize");
        assert_eq!(
            orcs.components.experimental,
            restored.components.experimental
        );
    }

    #[test]
    fn components_experimental_toml_parse() {
        let toml = r#"
[components]
experimental = ["custom_exp", "another_exp"]
"#;
        let config = OrcsConfig::from_toml(toml).expect("should parse");
        assert_eq!(
            config.components.experimental,
            vec!["custom_exp", "another_exp"]
        );
    }

    #[test]
    fn components_merge_experimental() {
        let mut base = ComponentsConfig::default();
        let overlay = ComponentsConfig {
            experimental: vec!["custom_exp".into()],
            ..Default::default()
        };
        base.merge(&overlay);
        assert_eq!(base.experimental, vec!["custom_exp"]);
    }

    // === ComponentsConfig settings tests ===

    #[test]
    fn components_settings_default_empty() {
        let config = ComponentsConfig::default();
        assert!(config.settings.is_empty());
    }

    #[test]
    fn components_settings_lookup_missing_returns_empty_object() {
        let config = ComponentsConfig::default();
        let settings = config.component_settings("nonexistent");
        assert_eq!(settings, serde_json::json!({}));
    }

    #[test]
    fn components_settings_lookup_existing() {
        let mut config = ComponentsConfig::default();
        config.settings.insert(
            "skill_manager".into(),
            serde_json::json!({ "recommend_skill": true }),
        );
        let settings = config.component_settings("skill_manager");
        assert_eq!(settings, serde_json::json!({ "recommend_skill": true }));
    }

    #[test]
    fn components_settings_toml_parse() {
        let toml = r#"
[components.settings.skill_manager]
recommend_skill = false

[components.settings.agent_mgr]
max_history = 20
"#;
        let config = OrcsConfig::from_toml(toml).expect("should parse settings");
        assert_eq!(
            config.components.component_settings("skill_manager"),
            serde_json::json!({ "recommend_skill": false })
        );
        assert_eq!(
            config.components.component_settings("agent_mgr"),
            serde_json::json!({ "max_history": 20 })
        );
        // Unknown component returns empty
        assert_eq!(
            config.components.component_settings("unknown"),
            serde_json::json!({})
        );
    }

    #[test]
    fn components_settings_merge_per_key() {
        let mut base = ComponentsConfig::default();
        base.settings.insert(
            "skill_manager".into(),
            serde_json::json!({ "recommend_skill": true }),
        );
        base.settings
            .insert("agent_mgr".into(), serde_json::json!({ "max_history": 10 }));

        let mut overlay = ComponentsConfig::default();
        // Override skill_manager, leave agent_mgr untouched
        overlay.settings.insert(
            "skill_manager".into(),
            serde_json::json!({ "recommend_skill": false }),
        );

        base.merge(&overlay);

        assert_eq!(
            base.component_settings("skill_manager"),
            serde_json::json!({ "recommend_skill": false }),
        );
        // agent_mgr preserved from base
        assert_eq!(
            base.component_settings("agent_mgr"),
            serde_json::json!({ "max_history": 10 }),
        );
    }

    #[test]
    fn components_settings_toml_roundtrip() {
        let mut config = OrcsConfig::default();
        config.components.settings.insert(
            "skill_manager".into(),
            serde_json::json!({ "recommend_skill": true }),
        );
        let toml = config.to_toml().expect("should serialize");
        let restored = OrcsConfig::from_toml(&toml).expect("should deserialize");
        assert_eq!(
            restored.components.component_settings("skill_manager"),
            serde_json::json!({ "recommend_skill": true })
        );
    }

    // === global_config_for_lua tests ===

    #[test]
    fn global_config_for_lua_default() {
        let config = OrcsConfig::default();
        let global = config.global_config_for_lua();

        assert_eq!(global["debug"], false);
        assert_eq!(global["model"]["default"], "llama3.2:latest");
        assert_eq!(global["model"]["temperature"], 0.7_f32 as f64);
        assert!(global["model"]["max_tokens"].is_null());
        assert_eq!(global["hil"]["auto_approve"], false);
        assert_eq!(global["hil"]["timeout_ms"], 30_000);
        assert_eq!(global["ui"]["verbose"], false);
        assert_eq!(global["ui"]["color"], true);
        assert_eq!(global["ui"]["emoji"], false);
    }

    #[test]
    fn global_config_for_lua_custom_values() {
        let config = OrcsConfig {
            debug: true,
            model: ModelConfig {
                default: "gpt-4o".into(),
                temperature: 0.3,
                max_tokens: Some(8192),
            },
            ui: UiConfig {
                verbose: true,
                color: false,
                emoji: true,
            },
            ..Default::default()
        };
        let global = config.global_config_for_lua();

        assert_eq!(global["debug"], true);
        assert_eq!(global["model"]["default"], "gpt-4o");
        assert_eq!(global["model"]["max_tokens"], 8192);
        assert_eq!(global["ui"]["verbose"], true);
        assert_eq!(global["ui"]["color"], false);
        assert_eq!(global["ui"]["emoji"], true);
    }

    #[test]
    fn global_config_for_lua_excludes_sensitive_fields() {
        let config = OrcsConfig::default();
        let global = config.global_config_for_lua();

        // These should NOT be present
        assert!(global.get("paths").is_none());
        assert!(global.get("scripts").is_none());
        assert!(global.get("components").is_none());
        assert!(global.get("hooks").is_none());
    }

    // === PathsConfig history_file tests ===

    #[test]
    fn history_file_default_is_none() {
        let config = PathsConfig::default();
        assert!(config.history_file.is_none());
    }

    #[test]
    fn history_file_or_default_returns_home_orcs_history() {
        let config = PathsConfig::default();
        let path = config.history_file_or_default();
        assert!(
            path.ends_with(".orcs/history"),
            "default should end with .orcs/history, got: {:?}",
            path
        );
    }

    #[test]
    fn history_file_or_default_uses_override() {
        let config = PathsConfig {
            history_file: Some(PathBuf::from("/sandbox/history")),
            ..Default::default()
        };
        assert_eq!(
            config.history_file_or_default(),
            PathBuf::from("/sandbox/history")
        );
    }

    #[test]
    fn paths_merge_history_file() {
        let mut base = PathsConfig::default();
        let overlay = PathsConfig {
            history_file: Some(PathBuf::from("/overlay/history")),
            ..Default::default()
        };
        base.merge(&overlay);
        assert_eq!(base.history_file, Some(PathBuf::from("/overlay/history")));
    }

    #[test]
    fn paths_merge_history_file_preserves_base_when_overlay_none() {
        let mut base = PathsConfig {
            history_file: Some(PathBuf::from("/base/history")),
            ..Default::default()
        };
        let overlay = PathsConfig::default();
        base.merge(&overlay);
        assert_eq!(base.history_file, Some(PathBuf::from("/base/history")));
    }

    #[test]
    fn history_file_toml_roundtrip() {
        let mut config = OrcsConfig::default();
        config.paths.history_file = Some(PathBuf::from("/custom/history"));
        let toml = config.to_toml().expect("should serialize");
        let restored = OrcsConfig::from_toml(&toml).expect("should deserialize");
        assert_eq!(
            restored.paths.history_file,
            Some(PathBuf::from("/custom/history"))
        );
    }
}

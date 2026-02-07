//! Configuration management with hierarchical layering.
//!
//! # Architecture
//!
//! Configuration is loaded from multiple sources with priority-based merging:
//!
//! ```text
//! Priority (highest to lowest):
//!
//! ┌─────────────────────────────────────────┐
//! │  1. Environment Variables (ORCS_*)      │  Runtime override
//! ├─────────────────────────────────────────┤
//! │  2. Project Config (.orcs/config.toml)  │  Project-specific
//! ├─────────────────────────────────────────┤
//! │  3. Global Config (~/.orcs/config.toml) │  User defaults
//! ├─────────────────────────────────────────┤
//! │  4. Default Values (compile-time)       │  Fallback
//! └─────────────────────────────────────────┘
//! ```
//!
//! # Directory Structure
//!
//! ```text
//! ~/.orcs/                     # Global ORCS directory
//! ├── config.toml              # Global configuration
//! ├── sessions/                # Session data (separate from config)
//! │   └── {uuid}.json
//! └── cache/                   # Cache directory (future)
//!
//! <project>/.orcs/             # Project-local ORCS directory
//! ├── config.toml              # Project configuration (overrides global)
//! └── context.md               # Project context (future)
//! ```
//!
//! # Config vs Session Separation
//!
//! | Aspect | Config | Session |
//! |--------|--------|---------|
//! | Format | TOML | JSON |
//! | Scope | Global / Project | Per-conversation |
//! | Mutability | Rarely changed | Frequently updated |
//! | Contents | Settings, preferences | History, snapshots |
//!
//! # Usage
//!
//! ```ignore
//! use orcs_runtime::config::{OrcsConfig, ConfigLoader};
//!
//! // Load configuration with automatic layering
//! let config = ConfigLoader::new()
//!     .with_project_root("/path/to/project")
//!     .load()?;
//!
//! // Access settings
//! if config.debug {
//!     println!("Debug mode enabled");
//! }
//!
//! // Environment variable override
//! // ORCS_DEBUG=true overrides config.debug
//! ```
//!
//! # Environment Variables
//!
//! | Variable | Config Field | Type |
//! |----------|--------------|------|
//! | `ORCS_DEBUG` | `debug` | bool |
//! | `ORCS_MODEL` | `model.default` | String |
//! | `ORCS_AUTO_APPROVE` | `hil.auto_approve` | bool |
//! | `ORCS_SESSION_PATH` | `paths.session_dir` | PathBuf |
//! | `ORCS_SCRIPTS_AUTO_LOAD` | `scripts.auto_load` | bool |
//!
//! # Example Configuration
//!
//! ```toml
//! # ~/.orcs/config.toml
//!
//! # General settings
//! debug = false
//!
//! [model]
//! default = "claude-3-opus"
//! temperature = 0.7
//!
//! [hil]
//! auto_approve = false
//! timeout_ms = 30000
//!
//! [paths]
//! session_dir = "~/.orcs/sessions"
//!
//! [ui]
//! verbose = false
//! color = true
//!
//! [scripts]
//! dirs = ["~/.orcs/scripts", ".orcs/scripts"]
//! auto_load = true
//! ```

mod error;
mod loader;
mod resolver;
mod types;

pub use error::ConfigError;
pub use loader::{save_global_config, ConfigLoader};
pub use resolver::{ConfigResolver, NoOpResolver};
pub use types::{HilConfig, ModelConfig, OrcsConfig, PathsConfig, ScriptsConfig, UiConfig};

/// Default global config directory.
pub fn default_config_dir() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".orcs")
}

/// Default global config file path.
pub fn default_config_path() -> std::path::PathBuf {
    default_config_dir().join("config.toml")
}

/// Project config directory name.
pub const PROJECT_CONFIG_DIR: &str = ".orcs";

/// Project config file name.
pub const PROJECT_CONFIG_FILE: &str = "config.toml";

//! Profile definitions and loading.
//!
//! Profiles bundle config overrides and per-component settings
//! into a switchable unit.
//!
//! # TOML Format
//!
//! ```toml
//! [profile]
//! name = "rust-dev"
//! description = "Rust development mode"
//!
//! [config]
//! debug = true
//!
//! [config.model]
//! default = "claude-opus-4-6"
//!
//! [components.skill_manager]
//! activate = ["rust-dev", "git-workflow"]
//! deactivate = ["python-dev"]
//!
//! [components.agent_mgr]
//! default_model = "claude-opus-4-6"
//! ```
//!
//! # Component Addressing
//!
//! Components can be addressed by last-name (`skill_manager`)
//! or FQL (`skill::skill_manager`). Last-name is the default.

use super::ConfigError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::debug;

/// Profile directory name within `.orcs/` or `~/.orcs/`.
pub const PROFILES_DIR: &str = "profiles";

/// Profile definition parsed from TOML.
///
/// Contains config overrides (applied to `OrcsConfig`) and
/// per-component settings (distributed via EventBus).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProfileDef {
    /// Profile metadata.
    pub profile: ProfileMeta,

    /// Config overrides (partial `OrcsConfig`).
    ///
    /// These fields are merged into the active `OrcsConfig`
    /// using the same overlay semantics as project config.
    pub config: Option<super::OrcsConfig>,

    /// Per-component settings, keyed by component last-name or FQL.
    ///
    /// Each entry is an arbitrary TOML table sent to the target
    /// component as a `profile_apply` request payload.
    ///
    /// # Example
    ///
    /// ```toml
    /// [components.skill_manager]
    /// activate = ["rust-dev"]
    /// ```
    #[serde(default)]
    pub components: HashMap<String, toml::Table>,
}

/// Profile metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProfileMeta {
    /// Profile name (must match filename without extension).
    pub name: String,

    /// Human-readable description.
    #[serde(default)]
    pub description: String,
}

impl ProfileDef {
    /// Parses a profile from TOML string.
    ///
    /// # Errors
    ///
    /// Returns error if TOML parsing fails.
    pub fn from_toml(toml_str: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(toml_str)
    }

    /// Serializes to TOML string.
    ///
    /// # Errors
    ///
    /// Returns error if serialization fails.
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Returns the profile name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.profile.name
    }

    /// Returns the profile description.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.profile.description
    }

    /// Returns component setting keys.
    #[must_use]
    pub fn component_names(&self) -> Vec<&str> {
        self.components.keys().map(|k| k.as_str()).collect()
    }
}

/// Lightweight profile entry for listing (no body loaded).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProfileEntry {
    /// Profile name.
    pub name: String,
    /// Description.
    pub description: String,
    /// Source file path.
    pub path: PathBuf,
}

/// Discovers and loads profiles from filesystem directories.
///
/// Search order:
/// 1. Project profiles: `.orcs/profiles/`
/// 2. Global profiles: `~/.orcs/profiles/`
///
/// Project profiles take precedence over global profiles with the same name.
#[derive(Debug, Clone)]
pub struct ProfileStore {
    /// Search directories (in priority order).
    search_dirs: Vec<PathBuf>,
}

impl ProfileStore {
    /// Creates a new store with default search dirs.
    ///
    /// - `~/.orcs/profiles/` (global)
    /// - Optionally: `.orcs/profiles/` (project, if project_root given)
    #[must_use]
    pub fn new(project_root: Option<&Path>) -> Self {
        let mut dirs = Vec::new();

        // Project profiles (highest priority)
        if let Some(root) = project_root {
            dirs.push(root.join(".orcs").join(PROFILES_DIR));
        }

        // Global profiles
        if let Some(home) = dirs::home_dir() {
            dirs.push(home.join(".orcs").join(PROFILES_DIR));
        }

        Self { search_dirs: dirs }
    }

    /// Creates a store with explicit search directories.
    #[must_use]
    pub fn with_dirs(dirs: Vec<PathBuf>) -> Self {
        Self { search_dirs: dirs }
    }

    /// Lists available profiles (name + description + path).
    ///
    /// Scans all search directories for `.toml` files.
    /// Deduplicates by name (first found wins = highest priority).
    pub fn list(&self) -> Vec<ProfileEntry> {
        let mut entries = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for dir in &self.search_dirs {
            if !dir.is_dir() {
                continue;
            }

            let read_dir = match std::fs::read_dir(dir) {
                Ok(rd) => rd,
                Err(e) => {
                    debug!(dir = %dir.display(), error = %e, "Failed to read profiles dir");
                    continue;
                }
            };

            for entry in read_dir.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                    continue;
                }

                let stem = match path.file_stem().and_then(|s| s.to_str()) {
                    Some(s) => s.to_string(),
                    None => continue,
                };

                if seen.contains(&stem) {
                    continue; // Higher-priority dir already has this name
                }

                match Self::load_meta(&path) {
                    Ok(meta) => {
                        let profile_name = if meta.name.is_empty() {
                            stem.clone()
                        } else {
                            meta.name.clone()
                        };
                        entries.push(ProfileEntry {
                            name: profile_name,
                            description: meta.description.clone(),
                            path: path.clone(),
                        });
                        seen.insert(stem);
                    }
                    Err(e) => {
                        debug!(
                            path = %path.display(),
                            error = %e,
                            "Failed to parse profile metadata"
                        );
                    }
                }
            }
        }

        entries
    }

    /// Loads a profile by name.
    ///
    /// Searches all dirs in priority order for `{name}.toml`.
    ///
    /// # Errors
    ///
    /// Returns `ConfigError` if profile not found or parse fails.
    pub fn load(&self, name: &str) -> Result<ProfileDef, ConfigError> {
        let filename = format!("{name}.toml");

        for dir in &self.search_dirs {
            let path = dir.join(&filename);
            if path.exists() {
                return Self::load_from_path(&path);
            }
        }

        Err(ConfigError::ProfileNotFound {
            name: name.to_string(),
            searched: self.search_dirs.clone(),
        })
    }

    /// Loads profile metadata only (lightweight, no component settings).
    ///
    /// Used by [`list()`](Self::list) to avoid full deserialization.
    fn load_meta(path: &Path) -> Result<ProfileMeta, ConfigError> {
        #[derive(Deserialize)]
        struct MetaOnly {
            #[serde(default)]
            profile: ProfileMeta,
        }
        let content = std::fs::read_to_string(path).map_err(|e| ConfigError::read_file(path, e))?;
        let meta: MetaOnly =
            toml::from_str(&content).map_err(|e| ConfigError::parse_toml(path, e))?;
        Ok(meta.profile)
    }

    /// Loads a profile from an explicit file path.
    ///
    /// # Errors
    ///
    /// Returns `ConfigError` if read or parse fails.
    pub fn load_from_path(path: &Path) -> Result<ProfileDef, ConfigError> {
        let content = std::fs::read_to_string(path).map_err(|e| ConfigError::read_file(path, e))?;

        let mut def: ProfileDef =
            ProfileDef::from_toml(&content).map_err(|e| ConfigError::parse_toml(path, e))?;

        // Default name from filename if not set in TOML
        if def.profile.name.is_empty() {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                def.profile.name = stem.to_string();
            }
        }

        debug!(
            name = %def.profile.name,
            path = %path.display(),
            components = def.components.len(),
            "Loaded profile"
        );

        Ok(def)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_profile(dir: &Path, name: &str, content: &str) -> PathBuf {
        let profiles_dir = dir.join(PROFILES_DIR);
        std::fs::create_dir_all(&profiles_dir).expect("should create profiles directory");
        let path = profiles_dir.join(format!("{name}.toml"));
        std::fs::write(&path, content).expect("should write profile TOML file");
        path
    }

    #[test]
    fn parse_minimal_profile() {
        let toml = r#"
[profile]
name = "test"
"#;
        let def = ProfileDef::from_toml(toml).expect("should parse minimal profile TOML");
        assert_eq!(def.name(), "test");
        assert!(def.description().is_empty());
        assert!(def.config.is_none());
        assert!(def.components.is_empty());
    }

    #[test]
    fn parse_full_profile() {
        let toml = r#"
[profile]
name = "rust-dev"
description = "Rust development mode"

[config]
debug = true

[config.model]
default = "claude-opus-4-6"

[components.skill_manager]
activate = ["rust-dev", "git-workflow"]
deactivate = ["python-dev"]

[components.agent_mgr]
default_model = "claude-opus-4-6"
"#;
        let def = ProfileDef::from_toml(toml).expect("should parse full profile TOML");
        assert_eq!(def.name(), "rust-dev");
        assert_eq!(def.description(), "Rust development mode");

        // Config override
        let config = def
            .config
            .as_ref()
            .expect("should have config section in full profile");
        assert!(config.debug);
        assert_eq!(config.model.default, "claude-opus-4-6");

        // Component settings
        assert_eq!(def.components.len(), 2);
        assert!(def.components.contains_key("skill_manager"));
        assert!(def.components.contains_key("agent_mgr"));

        let sm = &def.components["skill_manager"];
        let activate = sm
            .get("activate")
            .expect("should have 'activate' key in skill_manager")
            .as_array()
            .expect("'activate' should be an array");
        assert_eq!(activate.len(), 2);
    }

    #[test]
    fn profile_roundtrip() {
        let toml = r#"
[profile]
name = "test"
description = "test profile"
"#;
        let def = ProfileDef::from_toml(toml).expect("should parse profile for roundtrip test");
        let serialized = def.to_toml().expect("should serialize profile to TOML");
        let restored =
            ProfileDef::from_toml(&serialized).expect("should deserialize roundtripped TOML");
        assert_eq!(def.profile, restored.profile);
    }

    #[test]
    fn store_list_profiles() {
        let temp = TempDir::new().expect("should create temp dir for store list test");

        write_profile(
            temp.path(),
            "alpha",
            r#"
[profile]
name = "alpha"
description = "First profile"
"#,
        );

        write_profile(
            temp.path(),
            "beta",
            r#"
[profile]
name = "beta"
description = "Second profile"
"#,
        );

        let store = ProfileStore::with_dirs(vec![temp.path().join(PROFILES_DIR)]);
        let entries = store.list();

        assert_eq!(entries.len(), 2);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
    }

    #[test]
    fn store_load_by_name() {
        let temp = TempDir::new().expect("should create temp dir for store load test");

        write_profile(
            temp.path(),
            "rust-dev",
            r#"
[profile]
name = "rust-dev"
description = "Rust development"

[components.skill_manager]
activate = ["rust-dev"]
"#,
        );

        let store = ProfileStore::with_dirs(vec![temp.path().join(PROFILES_DIR)]);
        let def = store
            .load("rust-dev")
            .expect("should load rust-dev profile by name");

        assert_eq!(def.name(), "rust-dev");
        assert_eq!(def.components.len(), 1);
    }

    #[test]
    fn store_load_not_found() {
        let temp = TempDir::new().expect("should create temp dir for not-found test");
        let store = ProfileStore::with_dirs(vec![temp.path().join(PROFILES_DIR)]);
        let result = store.load("nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn store_priority_first_wins() {
        let high = TempDir::new().expect("should create temp dir for high-priority profiles");
        let low = TempDir::new().expect("should create temp dir for low-priority profiles");

        write_profile(
            high.path(),
            "shared",
            r#"
[profile]
name = "shared"
description = "high priority"
"#,
        );

        write_profile(
            low.path(),
            "shared",
            r#"
[profile]
name = "shared"
description = "low priority"
"#,
        );

        let store = ProfileStore::with_dirs(vec![
            high.path().join(PROFILES_DIR),
            low.path().join(PROFILES_DIR),
        ]);

        let def = store
            .load("shared")
            .expect("should load shared profile from high-priority dir");
        assert_eq!(def.description(), "high priority");
    }

    #[test]
    fn name_defaults_to_filename() {
        let temp = TempDir::new().expect("should create temp dir for filename-default test");

        write_profile(
            temp.path(),
            "my-profile",
            r#"
[profile]
description = "No name field"
"#,
        );

        let store = ProfileStore::with_dirs(vec![temp.path().join(PROFILES_DIR)]);
        let def = store
            .load("my-profile")
            .expect("should load profile and default name to filename");
        assert_eq!(def.name(), "my-profile");
    }

    #[test]
    fn component_names() {
        let toml = r#"
[profile]
name = "test"

[components.skill_manager]
activate = ["a"]

[components."skill::skill_manager"]
fql_setting = true
"#;
        let def = ProfileDef::from_toml(toml).expect("should parse profile with component names");
        let names = def.component_names();
        assert_eq!(names.len(), 2);
    }
}

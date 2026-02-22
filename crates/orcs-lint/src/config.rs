use crate::types::Severity;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub analyzer: AnalyzerConfig,

    #[serde(default)]
    pub rules: HashMap<String, RuleConfig>,
}

impl Config {
    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        Self::parse(&content)
    }

    pub fn parse(content: &str) -> Result<Self, ConfigError> {
        toml::from_str(content).map_err(|e| ConfigError::Parse {
            message: e.to_string(),
        })
    }

    #[must_use]
    pub fn is_rule_enabled(&self, rule_name: &str) -> bool {
        self.rules
            .get(rule_name)
            .map_or(true, |c| c.enabled.unwrap_or(true))
    }

    #[must_use]
    pub fn rule_severity(&self, rule_name: &str) -> Option<Severity> {
        self.rules.get(rule_name).and_then(|c| c.severity)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyzerConfig {
    #[serde(default = "default_root")]
    pub root: PathBuf,

    #[serde(default)]
    pub exclude: Vec<String>,
}

impl Default for AnalyzerConfig {
    fn default() -> Self {
        Self {
            root: default_root(),
            exclude: vec!["**/target/**".to_string()],
        }
    }
}

fn default_root() -> PathBuf {
    PathBuf::from(".")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuleConfig {
    #[serde(default)]
    pub enabled: Option<bool>,

    #[serde(default)]
    pub severity: Option<Severity>,

    #[serde(flatten)]
    pub options: HashMap<String, toml::Value>,
}

impl RuleConfig {
    #[must_use]
    pub fn get_bool(&self, key: &str, default: bool) -> bool {
        self.options
            .get(key)
            .and_then(toml::Value::as_bool)
            .unwrap_or(default)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Failed to read config file {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("Failed to parse config: {message}")]
    Parse { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let config = Config::default();
        assert!(config.rules.is_empty());
        assert!(config.is_rule_enabled("any-rule"));
    }

    #[test]
    fn parse_config() {
        let toml = r#"
[analyzer]
root = "./crates"

[rules.no-unwrap]
enabled = true
severity = "warning"
allow_in_tests = true
"#;
        let config = Config::parse(toml).expect("parse failed");
        assert_eq!(config.analyzer.root, PathBuf::from("./crates"));
        assert!(config.is_rule_enabled("no-unwrap"));

        let rule_config = config.rules.get("no-unwrap").expect("rule not found");
        assert!(rule_config.get_bool("allow_in_tests", false));
        assert_eq!(rule_config.severity, Some(Severity::Warning));
    }

    #[test]
    fn disabled_rule() {
        let toml = r#"
[rules.no-unwrap]
enabled = false
"#;
        let config = Config::parse(toml).expect("parse failed");
        assert!(!config.is_rule_enabled("no-unwrap"));
    }
}

//! Configuration errors.

use std::path::PathBuf;
use thiserror::Error;

/// Configuration error type.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Failed to read config file.
    #[error("failed to read config file '{path}': {source}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Failed to parse TOML.
    #[error("failed to parse config file '{path}': {source}")]
    ParseToml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    /// Failed to serialize config.
    #[error("failed to serialize config: {0}")]
    Serialize(#[from] toml::ser::Error),

    /// Failed to write config file.
    #[error("failed to write config file '{path}': {source}")]
    WriteFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Invalid environment variable value.
    #[error("invalid value for environment variable '{name}': {message}")]
    InvalidEnvVar { name: String, message: String },

    /// Failed to create config directory.
    #[error("failed to create config directory '{path}': {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl ConfigError {
    /// Creates a read file error.
    pub fn read_file(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::ReadFile {
            path: path.into(),
            source,
        }
    }

    /// Creates a parse TOML error.
    pub fn parse_toml(path: impl Into<PathBuf>, source: toml::de::Error) -> Self {
        Self::ParseToml {
            path: path.into(),
            source,
        }
    }

    /// Creates a write file error.
    pub fn write_file(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::WriteFile {
            path: path.into(),
            source,
        }
    }

    /// Creates an invalid env var error.
    pub fn invalid_env_var(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::InvalidEnvVar {
            name: name.into(),
            message: message.into(),
        }
    }

    /// Creates a create dir error.
    pub fn create_dir(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::CreateDir {
            path: path.into(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display() {
        let err = ConfigError::invalid_env_var("ORCS_DEBUG", "expected bool");
        assert!(err.to_string().contains("ORCS_DEBUG"));
        assert!(err.to_string().contains("expected bool"));
    }
}

//! Configuration resolver trait.
//!
//! # Architecture
//!
//! The `ConfigResolver` trait abstracts configuration resolution.
//! Each call to `resolve()` produces a fully-merged `OrcsConfig`
//! from all available sources (files, env, CLI flags, etc.).
//!
//! The application layer holds a resolver and controls **when** to
//! call `resolve()`, enabling hot-reload without process restart.
//!
//! ```text
//! impl ConfigResolver (CLI / test / GUI / ...)
//!          │
//!          ▼
//!   resolve() → Result<OrcsConfig>
//!          │
//!          ▼
//!   OrcsApp holds resolver → reload_config() at any time
//! ```
//!
//! # Example
//!
//! ```ignore
//! use orcs_runtime::config::{ConfigResolver, ConfigLoader, OrcsConfig, ConfigError};
//!
//! struct MyResolver;
//!
//! impl ConfigResolver for MyResolver {
//!     fn resolve(&self) -> Result<OrcsConfig, ConfigError> {
//!         ConfigLoader::new().load()
//!     }
//! }
//! ```

use super::{ConfigError, OrcsConfig};

/// Trait for resolving configuration from all sources.
///
/// Each invocation of `resolve()` returns a fresh, fully-merged
/// `OrcsConfig`. This allows the application to re-resolve at any
/// time (e.g., after a config file edit) without restarting.
pub trait ConfigResolver: Send + Sync {
    /// Resolves and returns the current configuration.
    ///
    /// Implementations should merge all layers (file, env, flags)
    /// and return the final result.
    fn resolve(&self) -> Result<OrcsConfig, ConfigError>;
}

/// No-op resolver that returns default configuration.
///
/// Useful as a fallback or for testing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOpResolver;

impl ConfigResolver for NoOpResolver {
    fn resolve(&self) -> Result<OrcsConfig, ConfigError> {
        Ok(OrcsConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_resolver_returns_default() {
        let config = NoOpResolver.resolve().unwrap();
        assert_eq!(config, OrcsConfig::default());
    }

    #[test]
    fn custom_resolver() {
        struct TestResolver {
            debug: bool,
        }

        impl ConfigResolver for TestResolver {
            fn resolve(&self) -> Result<OrcsConfig, ConfigError> {
                Ok(OrcsConfig {
                    debug: self.debug,
                    ..OrcsConfig::default()
                })
            }
        }

        let config = TestResolver { debug: true }.resolve().unwrap();
        assert!(config.debug);
    }
}

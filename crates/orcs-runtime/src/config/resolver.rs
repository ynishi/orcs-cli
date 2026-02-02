//! Configuration resolver trait for layered overrides.
//!
//! # Architecture
//!
//! ```text
//! ConfigLoader.load()  →  OrcsConfig (base)
//!                              │
//!                              ▼
//!                     ConfigResolver.apply()
//!                              │
//!                              ▼
//!                     OrcsConfig (final)
//! ```
//!
//! # Example
//!
//! ```ignore
//! use orcs_runtime::config::{ConfigLoader, ConfigResolver, OrcsConfig};
//!
//! struct CliOverrides {
//!     verbose: Option<bool>,
//! }
//!
//! impl ConfigResolver for CliOverrides {
//!     fn apply(&self, config: &mut OrcsConfig) {
//!         if let Some(v) = self.verbose {
//!             config.ui.verbose = v;
//!         }
//!     }
//! }
//!
//! let mut config = ConfigLoader::new().load()?;
//! let cli = CliOverrides { verbose: Some(true) };
//! cli.apply(&mut config);
//! ```

use super::OrcsConfig;

/// Trait for applying configuration overrides.
///
/// Implementors can modify an existing config with their specific overrides.
/// This enables a clean separation between config loading (file/env) and
/// runtime overrides (CLI flags, programmatic settings).
pub trait ConfigResolver {
    /// Applies overrides to the given configuration.
    ///
    /// Only non-None values should be applied, preserving existing values
    /// for unspecified options.
    fn apply(&self, config: &mut OrcsConfig);
}

/// No-op resolver that makes no changes.
///
/// Useful as a default or for testing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOpResolver;

impl ConfigResolver for NoOpResolver {
    fn apply(&self, _config: &mut OrcsConfig) {
        // No changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_resolver_does_nothing() {
        let mut config = OrcsConfig::default();
        let original = config.clone();

        NoOpResolver.apply(&mut config);

        assert_eq!(config, original);
    }

    #[test]
    fn custom_resolver() {
        struct TestResolver {
            debug: Option<bool>,
        }

        impl ConfigResolver for TestResolver {
            fn apply(&self, config: &mut OrcsConfig) {
                if let Some(d) = self.debug {
                    config.debug = d;
                }
            }
        }

        let mut config = OrcsConfig::default();
        assert!(!config.debug);

        let resolver = TestResolver { debug: Some(true) };
        resolver.apply(&mut config);

        assert!(config.debug);
    }
}

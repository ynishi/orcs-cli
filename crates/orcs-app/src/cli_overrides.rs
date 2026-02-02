//! CLI configuration overrides.
//!
//! Implements [`ConfigResolver`] for CLI flag overrides.

use orcs_runtime::{ConfigResolver, OrcsConfig};
use std::path::PathBuf;

/// CLI configuration overrides.
///
/// Applied as the highest priority layer after file/env config loading.
///
/// # Example
///
/// ```ignore
/// use orcs_app::CliOverrides;
/// use orcs_runtime::{ConfigLoader, ConfigResolver};
///
/// let mut config = ConfigLoader::new().load()?;
///
/// let cli = CliOverrides::new()
///     .verbose(true)
///     .debug(true);
///
/// cli.apply(&mut config);
/// ```
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    verbose: Option<bool>,
    debug: Option<bool>,
    session_path: Option<PathBuf>,
}

impl CliOverrides {
    /// Creates a new empty overrides.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets verbose mode override.
    #[must_use]
    pub fn verbose(mut self, value: bool) -> Self {
        self.verbose = Some(value);
        self
    }

    /// Sets debug mode override.
    #[must_use]
    pub fn debug(mut self, value: bool) -> Self {
        self.debug = Some(value);
        self
    }

    /// Sets session path override.
    #[must_use]
    pub fn session_path(mut self, path: PathBuf) -> Self {
        self.session_path = Some(path);
        self
    }

    /// Sets verbose mode override if Some.
    #[must_use]
    pub fn verbose_opt(mut self, value: Option<bool>) -> Self {
        if let Some(v) = value {
            self.verbose = Some(v);
        }
        self
    }

    /// Sets debug mode override if Some.
    #[must_use]
    pub fn debug_opt(mut self, value: Option<bool>) -> Self {
        if let Some(v) = value {
            self.debug = Some(v);
        }
        self
    }

    /// Sets session path override if Some.
    #[must_use]
    pub fn session_path_opt(mut self, path: Option<PathBuf>) -> Self {
        if let Some(p) = path {
            self.session_path = Some(p);
        }
        self
    }
}

impl ConfigResolver for CliOverrides {
    fn apply(&self, config: &mut OrcsConfig) {
        if let Some(v) = self.verbose {
            config.ui.verbose = v;
        }
        if let Some(d) = self.debug {
            config.debug = d;
        }
        if let Some(ref p) = self.session_path {
            config.paths.session_dir = Some(p.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_applies_nothing() {
        let mut config = OrcsConfig::default();
        let original = config.clone();

        CliOverrides::new().apply(&mut config);

        assert_eq!(config, original);
    }

    #[test]
    fn verbose_override() {
        let mut config = OrcsConfig::default();
        assert!(!config.ui.verbose);

        CliOverrides::new().verbose(true).apply(&mut config);

        assert!(config.ui.verbose);
    }

    #[test]
    fn debug_override() {
        let mut config = OrcsConfig::default();
        assert!(!config.debug);

        CliOverrides::new().debug(true).apply(&mut config);

        assert!(config.debug);
    }

    #[test]
    fn session_path_override() {
        let mut config = OrcsConfig::default();
        assert!(config.paths.session_dir.is_none());

        let path = PathBuf::from("/custom/path");
        CliOverrides::new()
            .session_path(path.clone())
            .apply(&mut config);

        assert_eq!(config.paths.session_dir, Some(path));
    }

    #[test]
    fn chained_overrides() {
        let mut config = OrcsConfig::default();

        CliOverrides::new()
            .verbose(true)
            .debug(true)
            .session_path(PathBuf::from("/test"))
            .apply(&mut config);

        assert!(config.ui.verbose);
        assert!(config.debug);
        assert_eq!(config.paths.session_dir, Some(PathBuf::from("/test")));
    }

    #[test]
    fn opt_methods_skip_none() {
        let mut config = OrcsConfig::default();
        let original = config.clone();

        CliOverrides::new()
            .verbose_opt(None)
            .debug_opt(None)
            .session_path_opt(None)
            .apply(&mut config);

        assert_eq!(config, original);
    }
}

//! Package support for component customization.
//!
//! The [`Packageable`] trait enables components to install and uninstall
//! packages that modify their behavior.
//!
//! # Design
//!
//! Packageable is an **optional** trait. Components that don't implement it
//! cannot have packages installed.
//!
//! # Example
//!
//! ```
//! use orcs_component::{Packageable, Package, PackageInfo, PackageError};
//! use serde::{Serialize, Deserialize};
//!
//! #[derive(Serialize, Deserialize)]
//! struct DecoratorConfig {
//!     prefix: String,
//!     suffix: String,
//! }
//!
//! struct EchoComponent {
//!     prefix: String,
//!     suffix: String,
//!     installed_packages: Vec<PackageInfo>,
//! }
//!
//! impl Packageable for EchoComponent {
//!     fn list_packages(&self) -> &[PackageInfo] {
//!         &self.installed_packages
//!     }
//!
//!     fn install_package(&mut self, package: &Package) -> Result<(), PackageError> {
//!         let config: DecoratorConfig = package.to_content()?;
//!         self.prefix = config.prefix;
//!         self.suffix = config.suffix;
//!         self.installed_packages.push(package.info.clone());
//!         Ok(())
//!     }
//!
//!     fn uninstall_package(&mut self, package_id: &str) -> Result<(), PackageError> {
//!         self.installed_packages.retain(|p| p.id != package_id);
//!         self.prefix = String::new();
//!         self.suffix = String::new();
//!         Ok(())
//!     }
//! }
//! ```

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors that can occur during package operations.
#[derive(Debug, Error)]
pub enum PackageError {
    /// Serialization/deserialization failed.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Package not found.
    #[error("package not found: {0}")]
    NotFound(String),

    /// Package already installed.
    #[error("package already installed: {0}")]
    AlreadyInstalled(String),

    /// Invalid package data.
    #[error("invalid package: {0}")]
    Invalid(String),

    /// Component does not support packages.
    #[error("component does not support packages")]
    NotSupported,

    /// Install failed.
    #[error("install failed: {0}")]
    InstallFailed(String),

    /// Uninstall failed.
    #[error("uninstall failed: {0}")]
    UninstallFailed(String),
}

/// Current package format version.
pub const PACKAGE_VERSION: u32 = 1;

/// Package metadata.
///
/// Contains identifying information about a package.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageInfo {
    /// Unique package identifier.
    pub id: String,

    /// Human-readable name.
    pub name: String,

    /// Version string (semver recommended).
    pub version: String,

    /// Brief description.
    pub description: String,

    /// Whether the package is currently enabled.
    pub enabled: bool,
}

impl PackageInfo {
    /// Creates new package info.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        version: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            version: version.into(),
            description: description.into(),
            enabled: true,
        }
    }

    /// Creates package info with enabled state.
    #[must_use]
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}

/// A package that can be installed into a component.
///
/// Contains metadata and content that modifies component behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
    /// Package metadata.
    pub info: PackageInfo,

    /// Package format version.
    pub version: u32,

    /// Package content (component-specific configuration).
    pub content: serde_json::Value,
}

impl Package {
    /// Creates a new package from serializable content.
    ///
    /// # Arguments
    ///
    /// * `info` - Package metadata
    /// * `content` - The content to serialize
    ///
    /// # Errors
    ///
    /// Returns `PackageError::Serialization` if the content cannot be serialized.
    pub fn new<T: Serialize>(info: PackageInfo, content: &T) -> Result<Self, PackageError> {
        Ok(Self {
            info,
            version: PACKAGE_VERSION,
            content: serde_json::to_value(content)?,
        })
    }

    /// Creates a package with raw JSON content.
    #[must_use]
    pub fn from_value(info: PackageInfo, content: serde_json::Value) -> Self {
        Self {
            info,
            version: PACKAGE_VERSION,
            content,
        }
    }

    /// Deserializes the content.
    ///
    /// # Errors
    ///
    /// Returns `PackageError::Serialization` if deserialization fails.
    pub fn to_content<T: for<'de> Deserialize<'de>>(&self) -> Result<T, PackageError> {
        Ok(serde_json::from_value(self.content.clone())?)
    }

    /// Returns the package ID.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.info.id
    }
}

/// Declares whether a component supports package installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PackageSupport {
    /// Component supports package installation.
    Enabled,

    /// Component does not support packages (default).
    #[default]
    Disabled,
}

/// Trait for components that support package installation.
///
/// Implementing this trait allows a component to have packages
/// installed that modify its behavior.
///
/// # Contract
///
/// - `list_packages()` returns currently installed packages
/// - `install_package()` adds new behavior from a package
/// - `uninstall_package()` removes installed package behavior
pub trait Packageable {
    /// Returns the list of installed packages.
    fn list_packages(&self) -> &[PackageInfo];

    /// Installs a package.
    ///
    /// # Errors
    ///
    /// Returns `PackageError` if installation fails.
    fn install_package(&mut self, package: &Package) -> Result<(), PackageError>;

    /// Uninstalls a package by ID.
    ///
    /// # Errors
    ///
    /// Returns `PackageError` if uninstallation fails.
    fn uninstall_package(&mut self, package_id: &str) -> Result<(), PackageError>;

    /// Returns whether a package is installed.
    fn is_installed(&self, package_id: &str) -> bool {
        self.list_packages().iter().any(|p| p.id == package_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct TestConfig {
        value: String,
    }

    #[test]
    fn package_info_new() {
        let info = PackageInfo::new("test-pkg", "Test Package", "1.0.0", "A test package");
        assert_eq!(info.id, "test-pkg");
        assert_eq!(info.name, "Test Package");
        assert_eq!(info.version, "1.0.0");
        assert!(info.enabled);
    }

    #[test]
    fn package_info_with_enabled() {
        let info = PackageInfo::new("test-pkg", "Test", "1.0.0", "Test").with_enabled(false);
        assert!(!info.enabled);
    }

    #[test]
    fn package_roundtrip() {
        let info = PackageInfo::new("test-pkg", "Test", "1.0.0", "Test");
        let config = TestConfig {
            value: "hello".into(),
        };

        let package = Package::new(info, &config).expect("create package");
        let restored: TestConfig = package.to_content().expect("deserialize content");

        assert_eq!(config, restored);
    }

    #[test]
    fn package_from_value() {
        let info = PackageInfo::new("test-pkg", "Test", "1.0.0", "Test");
        let content = serde_json::json!({"key": "value"});

        let package = Package::from_value(info, content.clone());
        assert_eq!(package.content, content);
    }

    #[test]
    fn package_id() {
        let info = PackageInfo::new("my-package", "My Package", "1.0.0", "Test");
        let package = Package::from_value(info, serde_json::Value::Null);
        assert_eq!(package.id(), "my-package");
    }

    struct TestComponent {
        packages: Vec<PackageInfo>,
        config_value: String,
    }

    impl Packageable for TestComponent {
        fn list_packages(&self) -> &[PackageInfo] {
            &self.packages
        }

        fn install_package(&mut self, package: &Package) -> Result<(), PackageError> {
            if self.is_installed(package.id()) {
                return Err(PackageError::AlreadyInstalled(package.id().to_string()));
            }
            let config: TestConfig = package.to_content()?;
            self.config_value = config.value;
            self.packages.push(package.info.clone());
            Ok(())
        }

        fn uninstall_package(&mut self, package_id: &str) -> Result<(), PackageError> {
            if !self.is_installed(package_id) {
                return Err(PackageError::NotFound(package_id.to_string()));
            }
            self.packages.retain(|p| p.id != package_id);
            self.config_value = String::new();
            Ok(())
        }
    }

    #[test]
    fn packageable_install_uninstall() {
        let mut comp = TestComponent {
            packages: vec![],
            config_value: String::new(),
        };

        let info = PackageInfo::new("decorator", "Decorator", "1.0.0", "Add decoration");
        let config = TestConfig {
            value: "decorated".into(),
        };
        let package =
            Package::new(info, &config).expect("Package::new should create a valid package");

        // Install
        comp.install_package(&package)
            .expect("first install of 'decorator' package should succeed");
        assert!(comp.is_installed("decorator"));
        assert_eq!(comp.config_value, "decorated");
        assert_eq!(comp.list_packages().len(), 1);

        // Uninstall
        comp.uninstall_package("decorator")
            .expect("uninstall of installed 'decorator' package should succeed");
        assert!(!comp.is_installed("decorator"));
        assert_eq!(comp.config_value, "");
        assert!(comp.list_packages().is_empty());
    }

    #[test]
    fn packageable_already_installed() {
        let mut comp = TestComponent {
            packages: vec![],
            config_value: String::new(),
        };

        let info = PackageInfo::new("test", "Test", "1.0.0", "Test");
        let config = TestConfig { value: "a".into() };
        let package =
            Package::new(info, &config).expect("Package::new should create a valid test package");

        comp.install_package(&package)
            .expect("first install of 'test' package should succeed");
        let result = comp.install_package(&package);
        assert!(matches!(result, Err(PackageError::AlreadyInstalled(_))));
    }

    #[test]
    fn packageable_not_found() {
        let mut comp = TestComponent {
            packages: vec![],
            config_value: String::new(),
        };

        let result = comp.uninstall_package("nonexistent");
        assert!(matches!(result, Err(PackageError::NotFound(_))));
    }
}

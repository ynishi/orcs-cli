//! Package Management for OrcsEngine.
//!
//! This module provides package installation and management functionality
//! for components that implement the [`Packageable`] trait.
//!
//! # Design
//!
//! Functions operate on component collections directly, enabling:
//! - Easier testing without full engine setup
//! - Clear separation of concerns
//! - Reusability across different contexts

use super::error::EngineError;
use orcs_component::{Component, Package, PackageInfo, Packageable};
use orcs_types::ComponentId;
use std::collections::HashMap;
use tracing::info;

/// Retrieves a mutable reference to a component's Packageable implementation.
///
/// # Errors
///
/// - `EngineError::ComponentNotFound` - Component doesn't exist
/// - `EngineError::PackageNotSupported` - Component doesn't implement Packageable
fn get_packageable_mut<'a>(
    components: &'a mut HashMap<ComponentId, Box<dyn Component>>,
    component_id: &ComponentId,
) -> Result<&'a mut dyn Packageable, EngineError> {
    let component = components
        .get_mut(component_id)
        .ok_or_else(|| EngineError::ComponentNotFound(component_id.clone()))?;

    component
        .as_packageable_mut()
        .ok_or_else(|| EngineError::PackageNotSupported(component_id.clone()))
}

/// Collects package info from all registered components.
///
/// Only components that implement [`Packageable`] will report packages.
///
/// # Arguments
///
/// * `components` - Reference to the component map
///
/// # Returns
///
/// A map of component ID to list of installed packages.
pub fn collect_packages(
    components: &HashMap<ComponentId, Box<dyn Component>>,
) -> HashMap<ComponentId, Vec<PackageInfo>> {
    let mut packages = HashMap::with_capacity(components.len());

    for (id, component) in components {
        if let Some(packageable) = component.as_packageable() {
            let list = packageable.list_packages();
            if !list.is_empty() {
                packages.insert(id.clone(), list.to_vec());
            }
        }
    }

    packages
}

/// Installs a package into a specific component.
///
/// The component must implement [`Packageable`].
///
/// # Arguments
///
/// * `components` - Mutable reference to the component map
/// * `component_id` - The target component
/// * `package` - The package to install
///
/// # Errors
///
/// - `EngineError::ComponentNotFound` - Component doesn't exist
/// - `EngineError::PackageNotSupported` - Component doesn't support packages
/// - `EngineError::PackageFailed` - Installation failed
pub fn install_package(
    components: &mut HashMap<ComponentId, Box<dyn Component>>,
    component_id: &ComponentId,
    package: &Package,
) -> Result<(), EngineError> {
    let packageable = get_packageable_mut(components, component_id)?;

    packageable
        .install_package(package)
        .map_err(|e| EngineError::PackageFailed(e.to_string()))?;

    info!(
        "Package '{}' installed to component '{}'",
        package.id(),
        component_id
    );

    Ok(())
}

/// Uninstalls a package from a specific component.
///
/// # Arguments
///
/// * `components` - Mutable reference to the component map
/// * `component_id` - The target component
/// * `package_id` - The package ID to uninstall
///
/// # Errors
///
/// - `EngineError::ComponentNotFound` - Component doesn't exist
/// - `EngineError::PackageNotSupported` - Component doesn't support packages
/// - `EngineError::PackageFailed` - Uninstallation failed
pub fn uninstall_package(
    components: &mut HashMap<ComponentId, Box<dyn Component>>,
    component_id: &ComponentId,
    package_id: &str,
) -> Result<(), EngineError> {
    let packageable = get_packageable_mut(components, component_id)?;

    packageable
        .uninstall_package(package_id)
        .map_err(|e| EngineError::PackageFailed(e.to_string()))?;

    info!(
        "Package '{}' uninstalled from component '{}'",
        package_id, component_id
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_component::{ComponentError, PackageError, Packageable, Status};
    use orcs_event::{Request, Signal, SignalResponse};
    use serde_json::Value;

    /// A component that supports packages for testing.
    struct PackageableComponent {
        id: ComponentId,
        packages: Vec<PackageInfo>,
    }

    impl PackageableComponent {
        fn new(name: &str) -> Self {
            Self {
                id: ComponentId::builtin(name),
                packages: Vec::new(),
            }
        }
    }

    impl Component for PackageableComponent {
        fn id(&self) -> &ComponentId {
            &self.id
        }

        fn status(&self) -> Status {
            Status::Idle
        }

        fn on_request(&mut self, request: &Request) -> Result<Value, ComponentError> {
            Ok(request.payload.clone())
        }

        fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
            if signal.is_veto() {
                SignalResponse::Abort
            } else {
                SignalResponse::Handled
            }
        }

        fn abort(&mut self) {}

        fn as_packageable(&self) -> Option<&dyn Packageable> {
            Some(self)
        }

        fn as_packageable_mut(&mut self) -> Option<&mut dyn Packageable> {
            Some(self)
        }
    }

    impl Packageable for PackageableComponent {
        fn list_packages(&self) -> &[PackageInfo] {
            &self.packages
        }

        fn install_package(&mut self, package: &Package) -> Result<(), PackageError> {
            self.packages.push(package.info.clone());
            Ok(())
        }

        fn uninstall_package(&mut self, package_id: &str) -> Result<(), PackageError> {
            self.packages.retain(|p| p.id != package_id);
            Ok(())
        }
    }

    /// A component that does not support packages.
    struct NonPackageableComponent {
        id: ComponentId,
    }

    impl NonPackageableComponent {
        fn new(name: &str) -> Self {
            Self {
                id: ComponentId::builtin(name),
            }
        }
    }

    impl Component for NonPackageableComponent {
        fn id(&self) -> &ComponentId {
            &self.id
        }

        fn status(&self) -> Status {
            Status::Idle
        }

        fn on_request(&mut self, request: &Request) -> Result<Value, ComponentError> {
            Ok(request.payload.clone())
        }

        fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
            if signal.is_veto() {
                SignalResponse::Abort
            } else {
                SignalResponse::Handled
            }
        }

        fn abort(&mut self) {}
    }

    fn make_test_package(id: &str) -> Package {
        let info = PackageInfo::new(id, id, "1.0.0", "Test package");
        Package::from_value(info, Value::Null)
    }

    #[test]
    fn collect_packages_from_packageable() {
        let mut components: HashMap<ComponentId, Box<dyn Component>> = HashMap::new();
        let mut comp = PackageableComponent::new("pkg");
        comp.packages
            .push(PackageInfo::new("test-pkg", "Test", "1.0.0", "Test"));
        let id = comp.id.clone();
        components.insert(id.clone(), Box::new(comp));

        let packages = collect_packages(&components);

        assert_eq!(packages.len(), 1);
        assert!(packages.contains_key(&id));
        assert_eq!(packages[&id].len(), 1);
        assert_eq!(packages[&id][0].id, "test-pkg");
    }

    #[test]
    fn collect_packages_skips_non_packageable() {
        let mut components: HashMap<ComponentId, Box<dyn Component>> = HashMap::new();

        let non_pkg = NonPackageableComponent::new("non-pkg");
        let non_pkg_id = non_pkg.id.clone();
        components.insert(non_pkg_id, Box::new(non_pkg));

        let packages = collect_packages(&components);

        assert!(packages.is_empty());
    }

    #[test]
    fn collect_packages_skips_empty() {
        let mut components: HashMap<ComponentId, Box<dyn Component>> = HashMap::new();

        // Packageable but with no packages
        let comp = PackageableComponent::new("empty");
        let id = comp.id.clone();
        components.insert(id.clone(), Box::new(comp));

        let packages = collect_packages(&components);

        // Should not include components with empty package lists
        assert!(packages.is_empty());
    }

    #[test]
    fn install_package_success() {
        let mut components: HashMap<ComponentId, Box<dyn Component>> = HashMap::new();
        let comp = PackageableComponent::new("pkg");
        let id = comp.id.clone();
        components.insert(id.clone(), Box::new(comp));

        let package = make_test_package("new-pkg");
        let result = install_package(&mut components, &id, &package);

        assert!(result.is_ok());

        // Verify package was installed
        let packages = collect_packages(&components);
        assert_eq!(packages[&id].len(), 1);
        assert_eq!(packages[&id][0].id, "new-pkg");
    }

    #[test]
    fn install_package_component_not_found() {
        let mut components: HashMap<ComponentId, Box<dyn Component>> = HashMap::new();
        let missing_id = ComponentId::builtin("missing");

        let package = make_test_package("test");
        let result = install_package(&mut components, &missing_id, &package);

        assert!(result.is_err());
        assert!(matches!(result, Err(EngineError::ComponentNotFound(_))));
    }

    #[test]
    fn install_package_not_supported() {
        let mut components: HashMap<ComponentId, Box<dyn Component>> = HashMap::new();
        let comp = NonPackageableComponent::new("non-pkg");
        let id = comp.id.clone();
        components.insert(id.clone(), Box::new(comp));

        let package = make_test_package("test");
        let result = install_package(&mut components, &id, &package);

        assert!(result.is_err());
        assert!(matches!(result, Err(EngineError::PackageNotSupported(_))));
    }

    #[test]
    fn uninstall_package_success() {
        let mut components: HashMap<ComponentId, Box<dyn Component>> = HashMap::new();
        let mut comp = PackageableComponent::new("pkg");
        comp.packages
            .push(PackageInfo::new("to-remove", "Remove", "1.0.0", "Test"));
        let id = comp.id.clone();
        components.insert(id.clone(), Box::new(comp));

        let result = uninstall_package(&mut components, &id, "to-remove");

        assert!(result.is_ok());

        // Verify package was removed
        let packages = collect_packages(&components);
        assert!(packages.is_empty());
    }
}

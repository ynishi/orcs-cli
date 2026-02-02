//! Session Persistence for OrcsEngine.
//!
//! This module provides snapshot collection and restoration functionality
//! for component state persistence across sessions.
//!
//! # Design
//!
//! Functions call `Component::snapshot()` and `Component::restore()` directly,
//! avoiding the need for Request/Response overhead and dummy ChannelIds.
//!
//! # Behavior
//!
//! - Components that don't support snapshots (return `NotSupported`) are skipped
//! - Other errors during restore cause immediate failure with `SnapshotError`

use orcs_component::{Component, ComponentSnapshot, SnapshotError};
use orcs_types::ComponentId;
use std::collections::HashMap;
use tracing::{debug, error, info, warn};

/// Collects snapshots from all components.
///
/// Calls `Component::snapshot()` on each component.
/// Components that don't support snapshots are silently skipped.
///
/// # Arguments
///
/// * `components` - Reference to the component map
///
/// # Returns
///
/// A map of component FQN (fully qualified name) to snapshot.
/// Uses FQN (`namespace::name`) instead of full ID to enable
/// snapshot restoration across sessions (UUIDs differ between runs).
pub fn collect_snapshots(
    components: &HashMap<ComponentId, Box<dyn Component>>,
) -> HashMap<String, ComponentSnapshot> {
    let mut snapshots = HashMap::new();

    for (id, component) in components {
        match component.snapshot() {
            Ok(snapshot) => {
                debug!("Collected snapshot for component: {}", id.fqn());
                snapshots.insert(id.fqn(), snapshot);
            }
            Err(SnapshotError::NotSupported(_)) => {
                // Component doesn't support snapshots - skip silently
                debug!("Component {} does not support snapshots", id.fqn());
            }
            Err(e) => {
                warn!("Failed to collect snapshot from {}: {}", id.fqn(), e);
            }
        }
    }

    info!("Collected {} component snapshots", snapshots.len());
    snapshots
}

/// Restores components from snapshots.
///
/// Calls `Component::restore()` on each component that has a saved snapshot.
///
/// # Behavior
///
/// - Components returning `NotSupported` are logged as warnings and skipped
/// - Other errors cause immediate return with `SnapshotError::RestoreFailed`
/// - Missing components (not registered) are skipped with debug log
///
/// # Arguments
///
/// * `components` - Mutable reference to the component map
/// * `snapshots` - Map of component FQN to snapshot
///
/// # Returns
///
/// - `Ok(count)` - Number of successfully restored components
/// - `Err(SnapshotError)` - Restore failed for a component
pub fn restore_snapshots(
    components: &mut HashMap<ComponentId, Box<dyn Component>>,
    snapshots: &HashMap<String, ComponentSnapshot>,
) -> Result<usize, SnapshotError> {
    let mut restored = 0;

    for (fqn, snapshot) in snapshots {
        // Find component by FQN (namespace::name)
        let component_id = components.keys().find(|k| k.fqn() == *fqn).cloned();

        if let Some(id) = component_id {
            if let Some(component) = components.get_mut(&id) {
                match component.restore(snapshot) {
                    Ok(()) => {
                        debug!("Restored component: {}", id.fqn());
                        restored += 1;
                    }
                    Err(SnapshotError::NotSupported(_)) => {
                        warn!("Component {} does not support snapshot restore", id.fqn());
                    }
                    Err(e) => {
                        error!("Failed to restore component {}: {}", id.fqn(), e);
                        return Err(SnapshotError::RestoreFailed {
                            component: id.fqn(),
                            reason: e.to_string(),
                        });
                    }
                }
            }
        } else {
            debug!(
                "Component {} not registered, skipping snapshot restore",
                fqn
            );
        }
    }

    info!("Restored {} components from session", restored);
    Ok(restored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_component::{ComponentError, Status};
    use orcs_event::{Request, Signal, SignalResponse};
    use serde_json::Value;

    /// A component that supports snapshots for testing.
    struct SnapshottableComponent {
        id: ComponentId,
        counter: u64,
    }

    impl SnapshottableComponent {
        fn new(name: &str, initial_value: u64) -> Self {
            Self {
                id: ComponentId::builtin(name),
                counter: initial_value,
            }
        }
    }

    impl Component for SnapshottableComponent {
        fn id(&self) -> &ComponentId {
            &self.id
        }

        fn status(&self) -> Status {
            Status::Idle
        }

        fn on_request(&mut self, request: &Request) -> Result<Value, ComponentError> {
            // Increment counter on any request (for testing state changes)
            self.counter += 1;
            Ok(Value::Number(self.counter.into()))
        }

        fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
            if signal.is_veto() {
                SignalResponse::Abort
            } else {
                SignalResponse::Handled
            }
        }

        fn abort(&mut self) {}

        fn snapshot(&self) -> Result<ComponentSnapshot, SnapshotError> {
            ComponentSnapshot::from_state(self.id.fqn(), &self.counter)
        }

        fn restore(&mut self, snapshot: &ComponentSnapshot) -> Result<(), SnapshotError> {
            self.counter = snapshot.to_state()?;
            Ok(())
        }
    }

    /// A component that does not support snapshots (uses default implementation).
    struct NonSnapshottableComponent {
        id: ComponentId,
    }

    impl NonSnapshottableComponent {
        fn new(name: &str) -> Self {
            Self {
                id: ComponentId::builtin(name),
            }
        }
    }

    impl Component for NonSnapshottableComponent {
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
        // Uses default snapshot() and restore() which return NotSupported
    }

    #[test]
    fn collect_snapshots_from_snapshottable() {
        let mut components: HashMap<ComponentId, Box<dyn Component>> = HashMap::new();
        let comp = SnapshottableComponent::new("snap", 42);
        let id = comp.id.clone();
        components.insert(id, Box::new(comp));

        let snapshots = collect_snapshots(&components);

        assert_eq!(snapshots.len(), 1);
        assert!(snapshots.contains_key("builtin::snap"));
    }

    #[test]
    fn collect_snapshots_skips_non_snapshottable() {
        let mut components: HashMap<ComponentId, Box<dyn Component>> = HashMap::new();

        let snap = SnapshottableComponent::new("snap", 42);
        let snap_id = snap.id.clone();
        components.insert(snap_id, Box::new(snap));

        let non_snap = NonSnapshottableComponent::new("echo");
        let non_snap_id = non_snap.id.clone();
        components.insert(non_snap_id, Box::new(non_snap));

        let snapshots = collect_snapshots(&components);

        // Only snapshottable component should have snapshot
        assert_eq!(snapshots.len(), 1);
        assert!(snapshots.contains_key("builtin::snap"));
    }

    #[test]
    fn restore_snapshots_success() {
        let mut components: HashMap<ComponentId, Box<dyn Component>> = HashMap::new();
        let comp = SnapshottableComponent::new("snap", 0);
        let id = comp.id.clone();
        components.insert(id, Box::new(comp));

        // Create snapshot with different value
        let mut snapshots = HashMap::new();
        let snapshot = ComponentSnapshot::from_state("builtin::snap", &100u64).unwrap();
        snapshots.insert("builtin::snap".to_string(), snapshot);

        let restored = restore_snapshots(&mut components, &snapshots).unwrap();

        assert_eq!(restored, 1);
    }

    #[test]
    fn restore_snapshots_skips_missing_component() {
        let mut components: HashMap<ComponentId, Box<dyn Component>> = HashMap::new();

        // Snapshot for non-existent component
        let mut snapshots = HashMap::new();
        let snapshot = ComponentSnapshot::from_state("builtin::missing", &42u64).unwrap();
        snapshots.insert("builtin::missing".to_string(), snapshot);

        let restored = restore_snapshots(&mut components, &snapshots).unwrap();

        assert_eq!(restored, 0);
    }

    #[test]
    fn roundtrip_snapshot() {
        let mut components: HashMap<ComponentId, Box<dyn Component>> = HashMap::new();
        let comp = SnapshottableComponent::new("snap", 42);
        let id = comp.id.clone();
        components.insert(id.clone(), Box::new(comp));

        // Collect snapshots
        let snapshots = collect_snapshots(&components);
        assert_eq!(snapshots.len(), 1);

        // Modify the component (triggers counter increment)
        if let Some(comp) = components.get_mut(&id) {
            let _ = comp.on_request(&Request::new(
                orcs_component::EventCategory::Echo,
                "increment",
                id.clone(),
                orcs_types::ChannelId::new(),
                Value::Null,
            ));
        }

        // Restore from snapshot (should reset counter to 42)
        let restored = restore_snapshots(&mut components, &snapshots).unwrap();
        assert_eq!(restored, 1);
    }
}

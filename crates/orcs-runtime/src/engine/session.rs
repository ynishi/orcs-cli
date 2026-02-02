//! Session Persistence for OrcsEngine.
//!
//! This module provides snapshot collection and restoration functionality
//! for component state persistence across sessions.
//!
//! # Design
//!
//! Functions operate on component collections directly, enabling:
//! - Easier testing without full engine setup
//! - Clear separation of concerns
//! - Reusability across different contexts

use orcs_component::{Component, ComponentSnapshot, EventCategory, SnapshotError};
use orcs_event::Request;
use orcs_types::{ChannelId, ComponentId};
use std::collections::HashMap;
use tracing::{debug, error, info, warn};

/// Lifecycle operation: snapshot collection.
pub(super) const OP_SNAPSHOT: &str = "snapshot";

/// Lifecycle operation: state restoration.
pub(super) const OP_RESTORE: &str = "restore";

/// Collects snapshots from all components via Lifecycle Request.
///
/// Sends `Request { category: Lifecycle, operation: "snapshot" }` to each component.
/// Components that support snapshots return `Ok(Value)` containing the snapshot.
/// Components that don't support snapshots return `Err(NotSupported)` and are skipped.
///
/// # Arguments
///
/// * `components` - Mutable reference to the component map
///
/// # Returns
///
/// A map of component FQN (fully qualified name) to snapshot.
/// Uses FQN (`namespace::name`) instead of full ID to enable
/// snapshot restoration across sessions (UUIDs differ between runs).
pub fn collect_snapshots(
    components: &mut HashMap<ComponentId, Box<dyn Component>>,
) -> HashMap<String, ComponentSnapshot> {
    let mut snapshots = HashMap::new();

    // Collect component IDs first to avoid borrow issues
    let component_ids: Vec<ComponentId> = components.keys().cloned().collect();

    for id in component_ids {
        if let Some(component) = components.get_mut(&id) {
            let request = Request::new(
                EventCategory::Lifecycle,
                OP_SNAPSHOT,
                id.clone(),
                ChannelId::new(),
                serde_json::Value::Null,
            );

            match component.on_request(&request) {
                Ok(value) => {
                    // Deserialize snapshot from Value
                    match serde_json::from_value::<ComponentSnapshot>(value) {
                        Ok(snapshot) => {
                            debug!("Collected snapshot for component: {}", id.fqn());
                            snapshots.insert(id.fqn(), snapshot);
                        }
                        Err(e) => {
                            warn!("Failed to deserialize snapshot from {}: {}", id.fqn(), e);
                        }
                    }
                }
                Err(orcs_component::ComponentError::NotSupported(_)) => {
                    // Component doesn't support snapshots - skip silently
                    debug!("Component {} does not support snapshots", id.fqn());
                }
                Err(e) => {
                    warn!("Failed to collect snapshot from {}: {}", id.fqn(), e);
                }
            }
        }
    }

    info!("Collected {} component snapshots", snapshots.len());
    snapshots
}

/// Restores components from snapshots via Lifecycle Request.
///
/// Sends `Request { category: Lifecycle, operation: "restore", payload: snapshot }`
/// to each component that has a saved snapshot.
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
                // Serialize snapshot to Value for request payload
                let payload = match serde_json::to_value(snapshot) {
                    Ok(v) => v,
                    Err(e) => {
                        return Err(SnapshotError::RestoreFailed {
                            component: fqn.clone(),
                            reason: format!("Failed to serialize snapshot: {e}"),
                        });
                    }
                };

                let request = Request::new(
                    EventCategory::Lifecycle,
                    OP_RESTORE,
                    id.clone(),
                    ChannelId::new(),
                    payload,
                );

                match component.on_request(&request) {
                    Ok(_) => {
                        debug!("Restored component: {}", id.fqn());
                        restored += 1;
                    }
                    Err(orcs_component::ComponentError::NotSupported(_)) => {
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
    use orcs_event::Signal;
    use orcs_event::SignalResponse;
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
            match request.operation.as_str() {
                OP_SNAPSHOT => {
                    let snapshot = ComponentSnapshot::from_state(self.id.fqn(), &self.counter)
                        .map_err(|e| ComponentError::ExecutionFailed(e.to_string()))?;
                    serde_json::to_value(snapshot)
                        .map_err(|e| ComponentError::ExecutionFailed(e.to_string()))
                }
                OP_RESTORE => {
                    let snapshot: ComponentSnapshot =
                        serde_json::from_value(request.payload.clone())
                            .map_err(|e| ComponentError::ExecutionFailed(e.to_string()))?;
                    self.counter = snapshot
                        .to_state()
                        .map_err(|e| ComponentError::ExecutionFailed(e.to_string()))?;
                    Ok(Value::Null)
                }
                _ => {
                    self.counter += 1;
                    Ok(Value::Number(self.counter.into()))
                }
            }
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

    /// A component that does not support snapshots.
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
            match request.operation.as_str() {
                OP_SNAPSHOT | OP_RESTORE => {
                    Err(ComponentError::NotSupported(request.operation.clone()))
                }
                _ => Ok(request.payload.clone()),
            }
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

    #[test]
    fn collect_snapshots_from_snapshottable() {
        let mut components: HashMap<ComponentId, Box<dyn Component>> = HashMap::new();
        let comp = SnapshottableComponent::new("snap", 42);
        let id = comp.id.clone();
        components.insert(id, Box::new(comp));

        let snapshots = collect_snapshots(&mut components);

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

        let snapshots = collect_snapshots(&mut components);

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
        let snapshots = collect_snapshots(&mut components);
        assert_eq!(snapshots.len(), 1);

        // Modify the component
        if let Some(comp) = components.get_mut(&id) {
            let _ = comp.on_request(&Request::new(
                EventCategory::Echo,
                "increment",
                id.clone(),
                ChannelId::new(),
                Value::Null,
            ));
        }

        // Restore from snapshot
        let restored = restore_snapshots(&mut components, &snapshots).unwrap();
        assert_eq!(restored, 1);
    }
}

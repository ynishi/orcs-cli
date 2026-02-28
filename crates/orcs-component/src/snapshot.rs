//! Component snapshot support for persistence and resume.
//!
//! The [`Snapshottable`] trait enables components to save and restore their state,
//! supporting session persistence and resume functionality.
//!
//! # Design
//!
//! Snapshottable is an **optional** trait. Components that don't implement it
//! will not have their state persisted across sessions.
//!
//! # Example
//!
//! ```
//! use orcs_component::{Snapshottable, ComponentSnapshot, SnapshotError};
//! use serde::{Serialize, Deserialize};
//!
//! #[derive(Serialize, Deserialize)]
//! struct CounterState {
//!     count: u64,
//! }
//!
//! struct CounterComponent {
//!     count: u64,
//! }
//!
//! impl Snapshottable for CounterComponent {
//!     fn snapshot(&self) -> ComponentSnapshot {
//!         let state = CounterState { count: self.count };
//!         ComponentSnapshot::from_state("counter", &state).expect("state should serialize")
//!     }
//!
//!     fn restore(&mut self, snapshot: &ComponentSnapshot) -> Result<(), SnapshotError> {
//!         let state: CounterState = snapshot.to_state()?;
//!         self.count = state.count;
//!         Ok(())
//!     }
//! }
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

/// Errors that can occur during snapshot operations.
#[derive(Debug, Error)]
pub enum SnapshotError {
    /// Serialization failed.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Snapshot version mismatch.
    #[error("version mismatch: expected {expected}, got {actual}")]
    VersionMismatch { expected: u32, actual: u32 },

    /// Component ID mismatch.
    #[error("component mismatch: expected {expected}, got {actual}")]
    ComponentMismatch { expected: String, actual: String },

    /// Invalid snapshot data.
    #[error("invalid snapshot data: {0}")]
    InvalidData(String),

    /// Component does not support snapshots.
    #[error("component {0} does not support snapshots")]
    NotSupported(String),

    /// Restore failed for a required component.
    #[error("restore failed for component {component}: {reason}")]
    RestoreFailed { component: String, reason: String },
}

/// Declares whether a component supports snapshot persistence.
///
/// Components use this to declare their snapshot capability.
/// The engine uses this to determine how to handle snapshot operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SnapshotSupport {
    /// Component supports and requires snapshot persistence.
    ///
    /// - `snapshot()` will be called during session save
    /// - `restore()` will be called during session load
    /// - **Restore failure is an error** (not a warning)
    Enabled,

    /// Component does not support snapshots (default).
    ///
    /// - `snapshot()` and `restore()` will NOT be called
    /// - If called anyway, returns `SnapshotError::NotSupported`
    #[default]
    Disabled,
}

/// Current snapshot format version.
pub const SNAPSHOT_VERSION: u32 = 1;

/// Component state snapshot.
///
/// Stores a component's serialized state along with metadata
/// for safe restoration.
///
/// # Fields
///
/// - `component_fqn`: Fully qualified name (`namespace::name`) for cross-session matching
/// - `version`: Format version for compatibility checking
/// - `state`: Serialized component state
/// - `metadata`: Optional additional data (timestamps, checksums, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentSnapshot {
    /// Fully qualified name (`namespace::name`) for validation.
    ///
    /// Uses FQN instead of full `ComponentId` because UUIDs differ between sessions.
    pub component_fqn: String,

    /// Snapshot format version.
    pub version: u32,

    /// Serialized component state.
    pub state: serde_json::Value,

    /// Optional metadata.
    pub metadata: HashMap<String, serde_json::Value>,
}

impl ComponentSnapshot {
    /// Creates a new snapshot from serializable state.
    ///
    /// # Arguments
    ///
    /// * `fqn` - The component's fully qualified name (`namespace::name`)
    /// * `state` - The state to serialize
    ///
    /// # Errors
    ///
    /// Returns `SnapshotError::Serialization` if the state cannot be serialized.
    pub fn from_state<T: Serialize>(
        fqn: impl Into<String>,
        state: &T,
    ) -> Result<Self, SnapshotError> {
        Ok(Self {
            component_fqn: fqn.into(),
            version: SNAPSHOT_VERSION,
            state: serde_json::to_value(state)?,
            metadata: HashMap::new(),
        })
    }

    /// Creates an empty snapshot (for components with no state).
    #[must_use]
    pub fn empty(fqn: impl Into<String>) -> Self {
        Self {
            component_fqn: fqn.into(),
            version: SNAPSHOT_VERSION,
            state: serde_json::Value::Null,
            metadata: HashMap::new(),
        }
    }

    /// Deserializes the state.
    ///
    /// # Errors
    ///
    /// Returns `SnapshotError::Serialization` if deserialization fails.
    pub fn to_state<T: for<'de> Deserialize<'de>>(&self) -> Result<T, SnapshotError> {
        Ok(serde_json::from_value(self.state.clone())?)
    }

    /// Adds metadata.
    #[must_use]
    pub fn with_metadata(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }

    /// Validates that this snapshot matches the expected component.
    ///
    /// # Errors
    ///
    /// Returns an error if the FQN or version doesn't match.
    pub fn validate(&self, expected_fqn: &str) -> Result<(), SnapshotError> {
        if self.component_fqn != expected_fqn {
            return Err(SnapshotError::ComponentMismatch {
                expected: expected_fqn.to_string(),
                actual: self.component_fqn.clone(),
            });
        }

        if self.version != SNAPSHOT_VERSION {
            return Err(SnapshotError::VersionMismatch {
                expected: SNAPSHOT_VERSION,
                actual: self.version,
            });
        }

        Ok(())
    }

    /// Returns true if the state is empty/null.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.state.is_null()
    }
}

/// Trait for components that support state persistence.
///
/// Implementing this trait allows a component's state to be saved
/// when a session is paused and restored when resumed.
///
/// # Contract
///
/// - `snapshot()` must produce a complete representation of the component's state
/// - `restore()` must return the component to the exact state represented by the snapshot
/// - Restored components must behave identically to their pre-snapshot state
///
/// # Thread Safety
///
/// Both methods take `&self` / `&mut self` as appropriate and don't require
/// additional synchronization.
pub trait Snapshottable {
    /// Creates a snapshot of the current state.
    ///
    /// The snapshot should contain all information needed to restore
    /// the component to its current state.
    fn snapshot(&self) -> ComponentSnapshot;

    /// Restores state from a snapshot.
    ///
    /// # Errors
    ///
    /// Returns `SnapshotError` if restoration fails (version mismatch,
    /// invalid data, etc.).
    fn restore(&mut self, snapshot: &ComponentSnapshot) -> Result<(), SnapshotError>;

    /// Returns true if this component has meaningful state to persist.
    ///
    /// Components that always start fresh can return `false` to skip
    /// snapshot/restore operations.
    fn has_state(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct TestState {
        value: i32,
        name: String,
    }

    #[test]
    fn snapshot_roundtrip() {
        let state = TestState {
            value: 42,
            name: "test".into(),
        };

        let snapshot =
            ComponentSnapshot::from_state("test-component", &state).expect("create snapshot");
        let restored: TestState = snapshot.to_state().expect("deserialize state");

        assert_eq!(state, restored);
    }

    #[test]
    fn snapshot_validation() {
        let snapshot = ComponentSnapshot::empty("my-component");

        assert!(snapshot.validate("my-component").is_ok());
        assert!(snapshot.validate("other-component").is_err());
    }

    #[test]
    fn snapshot_with_metadata() {
        let snapshot = ComponentSnapshot::empty("test")
            .with_metadata("timestamp", serde_json::json!(12345))
            .with_metadata("version", serde_json::json!("1.0.0"));

        assert_eq!(snapshot.metadata.len(), 2);
        assert_eq!(snapshot.metadata["timestamp"], serde_json::json!(12345));
    }

    #[test]
    fn empty_snapshot() {
        let snapshot = ComponentSnapshot::empty("empty-component");
        assert!(snapshot.is_empty());
        assert_eq!(snapshot.component_fqn, "empty-component");
    }

    #[test]
    fn version_mismatch_error() {
        let mut snapshot = ComponentSnapshot::empty("test");
        snapshot.version = 999;

        let result = snapshot.validate("test");
        assert!(matches!(result, Err(SnapshotError::VersionMismatch { .. })));
    }

    struct TestComponent {
        count: u64,
    }

    impl Snapshottable for TestComponent {
        fn snapshot(&self) -> ComponentSnapshot {
            ComponentSnapshot::from_state("test", &self.count).expect("create snapshot")
        }

        fn restore(&mut self, snapshot: &ComponentSnapshot) -> Result<(), SnapshotError> {
            self.count = snapshot.to_state()?;
            Ok(())
        }
    }

    #[test]
    fn snapshottable_trait() {
        let mut comp = TestComponent { count: 100 };
        let snapshot = comp.snapshot();

        comp.count = 0;
        comp.restore(&snapshot).expect("restore snapshot");

        assert_eq!(comp.count, 100);
    }
}

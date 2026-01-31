//! Core traits for component identification and control.
//!
//! These are the **mandatory** traits that all components and children
//! must implement to participate in the ORCS system.
//!
//! # Trait Hierarchy
//!
//! ```text
//! Required for all:
//!   ├── Identifiable  (id)
//!   ├── SignalReceiver (on_signal, abort)
//!   └── Statusable    (status, status_detail)
//!
//! Combinations:
//!   ├── Child = Identifiable + SignalReceiver + Statusable
//!   └── Component = Child + on_request + init + shutdown
//! ```
//!
//! # Why These Three?
//!
//! 1. **Identifiable**: Everything must be identifiable for routing/logging
//! 2. **SignalReceiver**: Human control is fundamental ("Human as Superpower")
//! 3. **Statusable**: Manager components need to monitor children
//!
//! # Example
//!
//! ```
//! use orcs_component::{Identifiable, SignalReceiver, Statusable, Status};
//! use orcs_event::{Signal, SignalResponse};
//!
//! struct MyWorker {
//!     id: String,
//!     status: Status,
//! }
//!
//! impl Identifiable for MyWorker {
//!     fn id(&self) -> &str {
//!         &self.id
//!     }
//! }
//!
//! impl SignalReceiver for MyWorker {
//!     fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
//!         if signal.is_veto() {
//!             self.abort();
//!             SignalResponse::Abort
//!         } else {
//!             SignalResponse::Ignored
//!         }
//!     }
//!
//!     fn abort(&mut self) {
//!         self.status = Status::Aborted;
//!     }
//! }
//!
//! impl Statusable for MyWorker {
//!     fn status(&self) -> Status {
//!         self.status
//!     }
//! }
//! ```

use crate::{Status, StatusDetail};
use orcs_event::{Signal, SignalResponse};

/// Identifiable component or child.
///
/// All entities in ORCS must be identifiable for:
///
/// - **Routing**: Directing messages to the right target
/// - **Logging**: Attributing actions to actors
/// - **Debugging**: Tracing execution flow
///
/// # ID Format
///
/// IDs should be:
///
/// - Unique within their scope (e.g., within a manager)
/// - Human-readable for debugging
/// - Stable across restarts (for serialized entities)
///
/// # Example
///
/// ```
/// use orcs_component::Identifiable;
///
/// struct Task {
///     id: String,
///     description: String,
/// }
///
/// impl Identifiable for Task {
///     fn id(&self) -> &str {
///         &self.id
///     }
/// }
///
/// let task = Task {
///     id: "task-001".into(),
///     description: "Process files".into(),
/// };
/// assert_eq!(task.id(), "task-001");
/// ```
pub trait Identifiable {
    /// Returns the entity's identifier.
    ///
    /// This should be stable and unique within scope.
    fn id(&self) -> &str;
}

/// Signal receiver for human control.
///
/// All entities must respond to signals because
/// "Human is Superpower" - humans can always interrupt.
///
/// # Signal Priority
///
/// Signals are the **highest priority** messages in ORCS.
/// They must be processed immediately, even during active work.
///
/// # Required Responses
///
/// | Signal | Expected Response |
/// |--------|-------------------|
/// | Veto | `Abort` + call `abort()` |
/// | Cancel (in scope) | `Abort` or `Handled` |
/// | Cancel (out of scope) | `Ignored` |
/// | Pause | `Handled` + change state |
/// | Resume | `Handled` + resume work |
///
/// # Example
///
/// ```
/// use orcs_component::{SignalReceiver, Status};
/// use orcs_event::{Signal, SignalResponse};
///
/// struct Worker {
///     status: Status,
/// }
///
/// impl SignalReceiver for Worker {
///     fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
///         if signal.is_veto() {
///             self.abort();
///             SignalResponse::Abort
///         } else if signal.is_global() {
///             // Global signals we can't handle
///             SignalResponse::Ignored
///         } else {
///             SignalResponse::Handled
///         }
///     }
///
///     fn abort(&mut self) {
///         self.status = Status::Aborted;
///         // Clean up resources, cancel pending work, etc.
///     }
/// }
/// ```
pub trait SignalReceiver {
    /// Handle an incoming signal.
    ///
    /// Called for every signal that reaches this entity.
    /// Must check signal scope to determine relevance.
    ///
    /// # Returns
    ///
    /// - `Handled`: Signal was processed
    /// - `Ignored`: Signal not relevant (out of scope)
    /// - `Abort`: Entity is stopping due to signal
    fn on_signal(&mut self, signal: &Signal) -> SignalResponse;

    /// Immediate abort.
    ///
    /// Called when a Veto signal is received.
    /// Must stop all work immediately and clean up.
    ///
    /// # Contract
    ///
    /// After `abort()` is called:
    /// - No more work should be performed
    /// - Resources should be released
    /// - Status should become `Aborted`
    fn abort(&mut self);
}

/// Status reporting for monitoring.
///
/// Managers need to know the status of their children.
/// This enables:
///
/// - **UI display**: Show progress to user
/// - **Health monitoring**: Detect stuck/failed children
/// - **Orchestration**: Coordinate parallel work
///
/// # Status Lifecycle
///
/// ```text
/// Initializing → Idle ⇄ Running → Completed
///                  ↓         ↓
///                Paused    Error
///                  ↓         ↓
///               Aborted ← ───┘
/// ```
///
/// # Example
///
/// ```
/// use orcs_component::{Statusable, Status, StatusDetail, Progress};
///
/// struct Compiler {
///     status: Status,
///     files_compiled: u64,
///     total_files: u64,
/// }
///
/// impl Statusable for Compiler {
///     fn status(&self) -> Status {
///         self.status
///     }
///
///     fn status_detail(&self) -> Option<StatusDetail> {
///         Some(StatusDetail {
///             message: Some("Compiling...".into()),
///             progress: Some(Progress::new(self.files_compiled, Some(self.total_files))),
///             metadata: Default::default(),
///         })
///     }
/// }
/// ```
pub trait Statusable {
    /// Returns the current status.
    ///
    /// This should reflect the actual state of the entity.
    fn status(&self) -> Status;

    /// Returns detailed status information.
    ///
    /// Optional - returns `None` by default.
    /// Implement for UI display or debugging.
    fn status_detail(&self) -> Option<StatusDetail> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_types::{Principal, PrincipalId};

    struct TestEntity {
        id: String,
        status: Status,
    }

    impl TestEntity {
        fn new(id: &str) -> Self {
            Self {
                id: id.into(),
                status: Status::Idle,
            }
        }
    }

    impl Identifiable for TestEntity {
        fn id(&self) -> &str {
            &self.id
        }
    }

    impl SignalReceiver for TestEntity {
        fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
            if signal.is_veto() {
                self.abort();
                SignalResponse::Abort
            } else {
                SignalResponse::Ignored
            }
        }

        fn abort(&mut self) {
            self.status = Status::Aborted;
        }
    }

    impl Statusable for TestEntity {
        fn status(&self) -> Status {
            self.status
        }
    }

    #[test]
    fn identifiable() {
        let entity = TestEntity::new("test-entity");
        assert_eq!(entity.id(), "test-entity");
    }

    #[test]
    fn signal_receiver_veto() {
        let mut entity = TestEntity::new("test");
        let signal = Signal::veto(Principal::User(PrincipalId::new()));

        let response = entity.on_signal(&signal);
        assert_eq!(response, SignalResponse::Abort);
        assert_eq!(entity.status(), Status::Aborted);
    }

    #[test]
    fn statusable_default_detail() {
        let entity = TestEntity::new("test");
        assert!(entity.status_detail().is_none());
    }
}

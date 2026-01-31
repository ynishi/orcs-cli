//! Child trait for component-managed entities.
//!
//! Components may internally manage child entities. All children
//! must implement this trait to ensure proper signal handling.
//!
//! # Why Child Trait?
//!
//! Without this constraint, Components could hold arbitrary entities
//! that don't respond to signals. This would break the "Human as
//! Superpower" principle - Veto signals must reach everything.
//!
//! # Component-Child Relationship
//!
//! ```text
//! ┌─────────────────────────────────────────────┐
//! │            Component (e.g., LlmComponent)    │
//! │                                              │
//! │  on_signal(Signal) {                        │
//! │      // Forward to all children             │
//! │      for child in &mut self.children {      │
//! │          child.on_signal(&signal);          │
//! │      }                                       │
//! │  }                                           │
//! │                                              │
//! │  children: Vec<Box<dyn Child>>              │
//! │  ┌────────┐  ┌────────┐  ┌────────┐        │
//! │  │ Agent1 │  │ Agent2 │  │ Worker │        │
//! │  │impl    │  │impl    │  │impl    │        │
//! │  │ Child  │  │ Child  │  │ Child  │        │
//! │  └────────┘  └────────┘  └────────┘        │
//! └─────────────────────────────────────────────┘
//! ```
//!
//! # Domain-Specific Implementations
//!
//! Concrete child types are defined in domain crates:
//!
//! - `orcs-llm`: `Agent impl Child`
//! - `orcs-skill`: `Skill impl Child`
//! - etc.
//!
//! # Example
//!
//! ```
//! use orcs_component::{Child, Identifiable, SignalReceiver, Statusable, Status};
//! use orcs_event::{Signal, SignalResponse};
//!
//! // Domain-specific child implementation
//! struct MyAgent {
//!     id: String,
//!     status: Status,
//! }
//!
//! impl Identifiable for MyAgent {
//!     fn id(&self) -> &str {
//!         &self.id
//!     }
//! }
//!
//! impl SignalReceiver for MyAgent {
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
//! impl Statusable for MyAgent {
//!     fn status(&self) -> Status {
//!         self.status
//!     }
//! }
//!
//! // Mark as Child - now safe to hold in Component
//! impl Child for MyAgent {}
//! ```

use crate::{Identifiable, SignalReceiver, Statusable};

/// A child entity managed by a Component.
///
/// All entities held inside a Component must implement this trait.
/// This ensures they can:
///
/// - Be identified ([`Identifiable`])
/// - Respond to signals ([`SignalReceiver`])
/// - Report status ([`Statusable`])
///
/// # Object Safety
///
/// This trait is object-safe, allowing `Box<dyn Child>`.
///
/// # Signal Propagation
///
/// When a Component receives a signal, it MUST forward it to all children:
///
/// ```ignore
/// fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
///     // Handle self first
///     // ...
///
///     // Forward to ALL children
///     for child in &mut self.children {
///         child.on_signal(signal);
///     }
/// }
/// ```
///
/// # Type Parameter Alternative
///
/// For stronger typing, Components can use:
///
/// ```ignore
/// struct MyComponent<C: Child> {
///     children: Vec<C>,
/// }
/// ```
pub trait Child: Identifiable + SignalReceiver + Statusable + Send + Sync {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Status;
    use orcs_event::{Signal, SignalResponse};

    struct TestChild {
        id: String,
        status: Status,
    }

    impl Identifiable for TestChild {
        fn id(&self) -> &str {
            &self.id
        }
    }

    impl SignalReceiver for TestChild {
        fn on_signal(&mut self, _signal: &Signal) -> SignalResponse {
            SignalResponse::Ignored
        }

        fn abort(&mut self) {
            self.status = Status::Aborted;
        }
    }

    impl Statusable for TestChild {
        fn status(&self) -> Status {
            self.status
        }
    }

    impl Child for TestChild {}

    #[test]
    fn child_object_safety() {
        let child: Box<dyn Child> = Box::new(TestChild {
            id: "test".into(),
            status: Status::Idle,
        });

        assert_eq!(child.id(), "test");
        assert_eq!(child.status(), Status::Idle);
    }

    #[test]
    fn child_collection() {
        let children: Vec<Box<dyn Child>> = vec![
            Box::new(TestChild {
                id: "child-1".into(),
                status: Status::Running,
            }),
            Box::new(TestChild {
                id: "child-2".into(),
                status: Status::Idle,
            }),
        ];

        assert_eq!(children.len(), 2);
        assert_eq!(children[0].id(), "child-1");
        assert_eq!(children[1].id(), "child-2");
    }
}

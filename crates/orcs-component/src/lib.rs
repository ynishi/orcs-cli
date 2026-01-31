//! Component system for ORCS CLI.
//!
//! This crate provides the component abstraction layer for the ORCS
//! (Orchestrated Runtime for Collaborative Systems) architecture.
//!
//! # Crate Architecture
//!
//! This crate is part of the **Plugin SDK** layer:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    Plugin SDK Layer                          │
//! │  (External, SemVer stable, safe to depend on)               │
//! ├─────────────────────────────────────────────────────────────┤
//! │  orcs-types     : ID types, Principal, ErrorCode            │
//! │  orcs-event     : Signal, Request, Event                    │
//! │  orcs-component : Component trait (WIT target)  ◄── HERE    │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## WIT Target
//!
//! The [`Component`] trait is designed to be exportable as a
//! WebAssembly Interface Type (WIT) for WASM component plugins.
//! This enables:
//!
//! - **Language-agnostic plugins**: Write components in any language
//! - **Sandboxed execution**: WASM provides security isolation
//! - **Dynamic loading**: Load plugins at runtime without recompilation
//!
//! # Component Architecture Overview
//!
//! Components are the functional units that communicate via the EventBus:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────┐
//! │                            OrcsEngine                                │
//! │  ┌────────────────────────────────────────────────────────────────┐  │
//! │  │                          EventBus                              │  │
//! │  │   - Request/Response routing                                   │  │
//! │  │   - Signal dispatch (highest priority)                         │  │
//! │  └────────────────────────────────────────────────────────────────┘  │
//! └──────────────────────────────────────────────────────────────────────┘
//!           │ EventBus
//!           ├──────────────┬──────────────┬──────────────┐
//!           ▼              ▼              ▼              ▼
//!     ┌──────────┐   ┌──────────┐   ┌──────────┐   ┌──────────┐
//!     │   LLM    │   │  Tools   │   │   HIL    │   │  Skill   │
//!     │Component │   │Component │   │Component │   │Component │
//!     └──────────┘   └──────────┘   └──────────┘   └──────────┘
//!           │
//!           │ Internal management
//!           ▼
//!     ┌────────────────────────────────────────────────────────┐
//!     │                Child / Agent / Skill                   │
//!     │            (managed by parent Component)               │
//!     └────────────────────────────────────────────────────────┘
//! ```
//!
//! # Core Traits
//!
//! ## Required Traits (All Components/Children Must Implement)
//!
//! | Trait | Purpose |
//! |-------|---------|
//! | [`Identifiable`] | Entity identification |
//! | [`SignalReceiver`] | Human control (abort, cancel) |
//! | [`Statusable`] | Status reporting for monitoring |
//!
//! ## Component Traits
//!
//! | Trait | Purpose |
//! |-------|---------|
//! | [`Component`] | EventBus participant with request handling |
//! | [`Child`] | Managed entity (no direct EventBus access) |
//! | [`Agent`] | Reactive child (subscribes to events) |
//! | [`Skill`] | Auto-triggered child |
//!
//! # Human as Superpower
//!
//! A core principle of ORCS: Human controls from above.
//! All components MUST respond to signals:
//!
//! - **Veto**: Stop everything immediately
//! - **Cancel**: Stop operations in scope
//!
//! This is enforced by the [`SignalReceiver`] trait.
//!
//! # Example: Simple Component
//!
//! ```
//! use orcs_component::{Component, ComponentError, Status};
//! use orcs_event::{Request, Signal, SignalResponse};
//! use orcs_types::ComponentId;
//! use serde_json::Value;
//!
//! struct EchoComponent {
//!     id: ComponentId,
//!     status: Status,
//! }
//!
//! impl EchoComponent {
//!     fn new() -> Self {
//!         Self {
//!             id: ComponentId::builtin("echo"),
//!             status: Status::Idle,
//!         }
//!     }
//! }
//!
//! impl Component for EchoComponent {
//!     fn id(&self) -> &ComponentId {
//!         &self.id
//!     }
//!
//!     fn status(&self) -> Status {
//!         self.status
//!     }
//!
//!     fn on_request(&mut self, request: &Request) -> Result<Value, ComponentError> {
//!         self.status = Status::Running;
//!         match request.operation.as_str() {
//!             "echo" => {
//!                 self.status = Status::Idle;
//!                 Ok(request.payload.clone())
//!             }
//!             op => Err(ComponentError::NotSupported(op.into())),
//!         }
//!     }
//!
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
//! ```
//!
//! # Example: Child Entity
//!
//! ```
//! use orcs_component::{Child, Identifiable, SignalReceiver, Statusable, Status};
//! use orcs_event::{Signal, SignalResponse};
//!
//! struct Worker {
//!     id: String,
//!     status: Status,
//! }
//!
//! impl Identifiable for Worker {
//!     fn id(&self) -> &str {
//!         &self.id
//!     }
//! }
//!
//! impl SignalReceiver for Worker {
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
//! impl Statusable for Worker {
//!     fn status(&self) -> Status {
//!         self.status
//!     }
//! }
//!
//! // Mark as Child
//! impl Child for Worker {}
//! ```
//!
//! # Crate Structure
//!
//! - [`Component`] - EventBus participant trait
//! - [`Child`], [`Agent`], [`Skill`] - Managed entity traits
//! - [`Identifiable`], [`SignalReceiver`], [`Statusable`] - Core traits
//! - [`Status`], [`StatusDetail`], [`Progress`] - Status types
//! - [`ComponentError`] - Error types
//!
//! # Related Crates
//!
//! - [`orcs_types`] - Core identifier types (ComponentId, Principal, etc.)
//! - [`orcs_event`] - Event types (Signal, Request)
//! - `orcs-runtime` - Runtime layer (Session, EventBus)

mod child;
mod component;
mod error;
mod snapshot;
mod status;
mod traits;

// Re-export core traits
pub use traits::{Identifiable, SignalReceiver, Statusable};

// Re-export component traits
pub use child::Child;
pub use component::Component;

// Re-export status types
pub use status::{Progress, Status, StatusDetail};

// Re-export snapshot types
pub use snapshot::{
    ComponentSnapshot, SnapshotError, SnapshotSupport, Snapshottable, SNAPSHOT_VERSION,
};

// Re-export error types
pub use error::ComponentError;

// Re-export EventCategory for convenience
pub use orcs_event::EventCategory;

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_event::{Request, Signal, SignalResponse};
    use orcs_types::{ChannelId, ComponentId, ErrorCode, Principal, PrincipalId};
    use serde_json::Value;

    struct MockComponent {
        id: ComponentId,
        status: Status,
    }

    impl MockComponent {
        fn new(name: &str) -> Self {
            Self {
                id: ComponentId::builtin(name),
                status: Status::Idle,
            }
        }
    }

    impl Component for MockComponent {
        fn id(&self) -> &ComponentId {
            &self.id
        }

        fn status(&self) -> Status {
            self.status
        }

        fn on_request(&mut self, request: &Request) -> Result<Value, ComponentError> {
            match request.operation.as_str() {
                "echo" => Ok(request.payload.clone()),
                _ => Err(ComponentError::NotSupported(request.operation.clone())),
            }
        }

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

    #[test]
    fn mock_component_echo() {
        let mut comp = MockComponent::new("echo");
        let source = ComponentId::builtin("test");
        let channel = ChannelId::new();
        let req = Request::new(
            EventCategory::Echo,
            "echo",
            source,
            channel,
            Value::String("hello".into()),
        );

        assert_eq!(
            comp.on_request(&req).unwrap(),
            Value::String("hello".into())
        );
    }

    #[test]
    fn mock_component_unknown_operation() {
        let mut comp = MockComponent::new("test");
        let source = ComponentId::builtin("test");
        let channel = ChannelId::new();
        let req = Request::new(EventCategory::Echo, "unknown", source, channel, Value::Null);

        let result = comp.on_request(&req);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), "COMPONENT_NOT_SUPPORTED");
    }

    #[test]
    fn mock_component_abort_on_veto() {
        let mut comp = MockComponent::new("test");
        let signal = Signal::veto(Principal::User(PrincipalId::new()));

        let resp = comp.on_signal(&signal);
        assert_eq!(resp, SignalResponse::Abort);
        assert_eq!(comp.status(), Status::Aborted);
    }
}

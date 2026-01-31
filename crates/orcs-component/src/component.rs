//! Component trait for ORCS EventBus participants.
//!
//! Components are the functional units that run on Channels and communicate
//! via the EventBus. They handle requests, respond to signals, and may
//! manage child entities internally.
//!
//! # Component vs Child
//!
//! | Aspect | Component | Child |
//! |--------|-----------|-------|
//! | EventBus access | Direct | Via parent |
//! | Request handling | Yes | No |
//! | Lifecycle methods | Yes | No |
//! | Manager capability | Yes | No |
//!
//! # Component Hierarchy
//!
//! ```text
//! ┌───────────────────────────────────────────────────────────┐
//! │                    OrcsEngine (Core)                      │
//! │  - EventBus dispatch                                      │
//! │  - Channel/World management                               │
//! │  - Component Runner (poll-based)                          │
//! └───────────────────────────────────────────────────────────┘
//!                            │
//!        ┌───────────────────┼───────────────────┐
//!        │                   │                   │
//!        ▼                   ▼                   ▼
//!   ┌─────────────┐    ┌─────────────┐    ┌─────────────┐
//!   │   System    │    │   System    │    │   System    │
//!   │  Component  │    │  Component  │    │  Component  │
//!   └─────────────┘    └─────────────┘    └─────────────┘
//!         │
//!         │ Internal management (Engine doesn't see this)
//!         ▼
//!   ┌─────────────────────────────────────────────────────┐
//!   │              Child / Agent / Skill                  │
//!   │              (managed by Component)                 │
//!   └─────────────────────────────────────────────────────┘
//! ```
//!
//! # Builtin Components
//!
//! | Component | Category | Purpose |
//! |-----------|----------|---------|
//! | LlmComponent | `Llm` | Chat, complete, embed |
//! | ToolsComponent | `Tool` | Read, write, edit, bash |
//! | HilComponent | `Hil` | Human approval/rejection |
//! | SkillComponent | `Skill` | Auto-triggered skills |
//!
//! # Example
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
//!         match request.operation.as_str() {
//!             "echo" => Ok(request.payload.clone()),
//!             _ => Err(ComponentError::NotSupported(request.operation.clone())),
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

use crate::{ComponentError, EventCategory, Status, StatusDetail};
use orcs_event::{Request, Signal, SignalResponse};
use orcs_types::ComponentId;
use serde_json::Value;

/// Component trait for EventBus participants.
///
/// Components are the primary actors in the ORCS system.
/// They communicate via the EventBus using Request/Response patterns.
///
/// # Required Methods
///
/// | Method | Purpose |
/// |--------|---------|
/// | `id` | Component identification |
/// | `status` | Current execution status |
/// | `subscriptions` | Event categories to receive |
/// | `on_request` | Handle incoming requests |
/// | `on_signal` | Handle control signals |
/// | `abort` | Immediate termination |
///
/// # Subscription-based Routing
///
/// Components declare which [`EventCategory`] they subscribe to.
/// The EventBus routes requests only to subscribers of the matching category.
///
/// ```text
/// Component::subscriptions() -> [Hil, Echo]
///     │
///     ▼
/// EventBus::register(component, categories)
///     │
///     ▼
/// Request { category: Hil, operation: "submit" }
///     │
///     ▼ (routed only to Hil subscribers)
/// HilComponent::on_request()
/// ```
///
/// # Signal Handling Contract
///
/// Components **must** handle signals, especially:
///
/// - **Veto**: Must abort immediately
/// - **Cancel**: Must check scope and abort if applicable
///
/// This is the foundation of "Human as Superpower" -
/// humans can always interrupt any operation.
///
/// # Thread Safety
///
/// Components must be `Send + Sync` for concurrent access.
/// Use interior mutability patterns if needed.
pub trait Component: Send + Sync {
    /// Returns the component's identifier.
    ///
    /// Used for:
    /// - Request routing
    /// - Signal scope checking
    /// - Logging and debugging
    fn id(&self) -> &ComponentId;

    /// Returns the event categories this component subscribes to.
    ///
    /// The EventBus routes requests only to components that subscribe
    /// to the request's category.
    ///
    /// # Default
    ///
    /// Default implementation returns `[Lifecycle]` only.
    /// Override to receive requests from other categories.
    ///
    /// # Example
    ///
    /// ```ignore
    /// fn subscriptions(&self) -> &[EventCategory] {
    ///     &[EventCategory::Hil, EventCategory::Lifecycle]
    /// }
    /// ```
    fn subscriptions(&self) -> &[EventCategory] {
        &[EventCategory::Lifecycle]
    }

    /// Returns the current execution status.
    ///
    /// Called by the engine to monitor component health.
    fn status(&self) -> Status;

    /// Returns detailed status information.
    ///
    /// Optional - returns `None` by default.
    /// Override for UI display or debugging.
    fn status_detail(&self) -> Option<StatusDetail> {
        None
    }

    /// Handle an incoming request.
    ///
    /// Called when a request is routed to this component.
    ///
    /// # Arguments
    ///
    /// * `request` - The incoming request
    ///
    /// # Returns
    ///
    /// - `Ok(Value)` - Successful response payload
    /// - `Err(ComponentError)` - Operation failed
    ///
    /// # Example Operations
    ///
    /// | Component | Operations |
    /// |-----------|------------|
    /// | LLM | chat, complete, embed |
    /// | Tools | read, write, edit, bash |
    /// | HIL | approve, reject |
    fn on_request(&mut self, request: &Request) -> Result<Value, ComponentError>;

    /// Handle an incoming signal.
    ///
    /// Signals are highest priority - process immediately.
    ///
    /// # Required Handling
    ///
    /// - **Veto**: Must call `abort()` and return `Abort`
    /// - **Cancel (in scope)**: Should abort
    /// - **Other**: Check relevance and handle or ignore
    ///
    /// # Returns
    ///
    /// - `Handled`: Signal was processed
    /// - `Ignored`: Signal not relevant
    /// - `Abort`: Component is stopping
    fn on_signal(&mut self, signal: &Signal) -> SignalResponse;

    /// Immediate abort.
    ///
    /// Called on Veto signal. Must:
    ///
    /// - Stop all ongoing work immediately
    /// - Cancel pending operations
    /// - Release resources
    /// - Set status to `Aborted`
    ///
    /// # Contract
    ///
    /// After `abort()`:
    /// - Component will not receive more requests
    /// - Any pending responses should be dropped
    fn abort(&mut self);

    /// Initialize the component.
    ///
    /// Called once before the component receives any requests.
    /// Default implementation does nothing.
    ///
    /// # Errors
    ///
    /// Return `Err` if initialization fails.
    /// The component will not be registered with the EventBus.
    fn init(&mut self) -> Result<(), ComponentError> {
        Ok(())
    }

    /// Shutdown the component.
    ///
    /// Called when the engine is stopping.
    /// Default implementation does nothing.
    ///
    /// Should:
    /// - Clean up resources
    /// - Persist state if needed
    /// - Cancel background tasks
    fn shutdown(&mut self) {
        // Default: no-op
    }

    /// Returns this component as a [`Packageable`] if supported.
    ///
    /// Override this method in components that implement [`Packageable`]
    /// to enable package management.
    ///
    /// # Default
    ///
    /// Returns `None` - component does not support packages.
    fn as_packageable(&self) -> Option<&dyn crate::Packageable> {
        None
    }

    /// Returns this component as a mutable [`Packageable`] if supported.
    ///
    /// Override this method in components that implement [`Packageable`]
    /// to enable package installation/uninstallation.
    ///
    /// # Default
    ///
    /// Returns `None` - component does not support packages.
    fn as_packageable_mut(&mut self) -> Option<&mut dyn crate::Packageable> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_types::{ChannelId, ErrorCode, Principal, PrincipalId};

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
    fn component_echo() {
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
    fn component_not_supported() {
        let mut comp = MockComponent::new("test");
        let source = ComponentId::builtin("test");
        let channel = ChannelId::new();
        let req = Request::new(EventCategory::Echo, "unknown", source, channel, Value::Null);

        let result = comp.on_request(&req);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), "COMPONENT_NOT_SUPPORTED");
    }

    #[test]
    fn component_abort_on_veto() {
        let mut comp = MockComponent::new("test");
        let signal = Signal::veto(Principal::User(PrincipalId::new()));

        let resp = comp.on_signal(&signal);
        assert_eq!(resp, SignalResponse::Abort);
        assert_eq!(comp.status(), Status::Aborted);
    }

    #[test]
    fn component_init_default() {
        let mut comp = MockComponent::new("test");
        assert!(comp.init().is_ok());
    }

    #[test]
    fn component_status_detail_default() {
        let comp = MockComponent::new("test");
        assert!(comp.status_detail().is_none());
    }
}

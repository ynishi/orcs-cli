//! NoopComponent - Minimal component for channel binding.
//!
//! `NoopComponent` is a minimal Component implementation that:
//! - Accepts all requests and returns success
//! - Handles all signals appropriately
//!
//! Use this when a Channel needs a bound Component but no specific
//! functionality is required (e.g., IO channel that routes events elsewhere).

use orcs_component::{Component, ComponentError, Status};
use orcs_event::{EventCategory, Request, Signal, SignalResponse};
use orcs_types::ComponentId;
use serde_json::Value;

/// Minimal Component that does nothing but satisfy the 1:1 Channel binding.
///
/// # Example
///
/// ```ignore
/// use orcs_runtime::components::NoopComponent;
///
/// let component = NoopComponent::new("io");
/// engine.spawn_runner(channel_id, Box::new(component));
/// ```
pub struct NoopComponent {
    id: ComponentId,
    status: Status,
}

impl NoopComponent {
    /// Creates a new NoopComponent with the given name.
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            id: ComponentId::builtin(name),
            status: Status::Idle,
        }
    }
}

impl Component for NoopComponent {
    fn id(&self) -> &ComponentId {
        &self.id
    }

    fn subscriptions(&self) -> Vec<EventCategory> {
        vec![EventCategory::Lifecycle]
    }

    fn status(&self) -> Status {
        self.status
    }

    fn on_request(&mut self, request: &Request) -> Result<Value, ComponentError> {
        // Return the payload as-is (echo behavior)
        Ok(request.payload.clone())
    }

    fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
        if signal.is_veto() {
            self.status = Status::Aborted;
            SignalResponse::Abort
        } else {
            SignalResponse::Handled
        }
    }

    fn abort(&mut self) {
        self.status = Status::Aborted;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_types::{ChannelId, Principal, PrincipalId};

    #[test]
    fn noop_creation() {
        let comp = NoopComponent::new("test");
        assert_eq!(comp.id().name, "test");
        assert_eq!(comp.status(), Status::Idle);
    }

    #[test]
    fn noop_handles_request() {
        let mut comp = NoopComponent::new("test");
        let source = ComponentId::builtin("source");
        let channel = ChannelId::new();
        let req = Request::new(
            EventCategory::Echo,
            "echo",
            source,
            channel,
            Value::String("hello".into()),
        );

        let result = comp.on_request(&req);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Value::String("hello".into()));
    }

    #[test]
    fn noop_handles_veto() {
        let mut comp = NoopComponent::new("test");
        let signal = Signal::veto(Principal::User(PrincipalId::new()));

        let resp = comp.on_signal(&signal);
        assert_eq!(resp, SignalResponse::Abort);
        assert_eq!(comp.status(), Status::Aborted);
    }
}

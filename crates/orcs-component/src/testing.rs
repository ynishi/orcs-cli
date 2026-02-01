//! Component testing harness for unit tests.
//!
//! Provides a test harness for testing Component implementations
//! without requiring the full OrcsEngine infrastructure.
//!
//! # Features
//!
//! - Engine-independent component testing
//! - Request/Signal/Lifecycle processing
//! - Event logging for snapshot testing
//! - Deterministic synchronous execution
//!
//! # Example
//!
//! ```
//! use orcs_component::testing::{ComponentTestHarness, RequestRecord};
//! use orcs_component::{Component, ComponentError, Status, EventCategory};
//! use orcs_event::{Request, Signal, SignalResponse};
//! use orcs_types::ComponentId;
//! use serde_json::{json, Value};
//!
//! struct EchoComponent {
//!     id: ComponentId,
//!     status: Status,
//! }
//!
//! impl Component for EchoComponent {
//!     fn id(&self) -> &ComponentId { &self.id }
//!     fn status(&self) -> Status { self.status }
//!     fn on_request(&mut self, req: &Request) -> Result<Value, ComponentError> {
//!         Ok(req.payload.clone())
//!     }
//!     fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
//!         if signal.is_veto() {
//!             self.abort();
//!             SignalResponse::Abort
//!         } else {
//!             SignalResponse::Handled
//!         }
//!     }
//!     fn abort(&mut self) { self.status = Status::Aborted; }
//! }
//!
//! let echo = EchoComponent {
//!     id: ComponentId::builtin("echo"),
//!     status: Status::Idle,
//! };
//! let mut harness = ComponentTestHarness::new(echo);
//!
//! // Test request handling
//! let result = harness.request(EventCategory::Echo, "echo", json!({"msg": "hello"}));
//! assert!(result.is_ok());
//!
//! // Test signal handling
//! let response = harness.veto();
//! assert_eq!(response, SignalResponse::Abort);
//! ```

use crate::{Component, ComponentError, EventCategory, Status};
use orcs_event::{Signal, SignalResponse};
use orcs_types::{ChannelId, ComponentId, Principal};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Record of a request sent to a component.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    /// Operation name.
    pub operation: String,
    /// Event category.
    pub category: String,
    /// Result of the request.
    pub result: RequestResult,
}

/// Result of a request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum RequestResult {
    /// Request succeeded with a value.
    Ok(Value),
    /// Request failed with an error message.
    Err(String),
}

/// Record of a signal sent to a component.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalRecord {
    /// Signal kind (e.g., "Veto", "Cancel", "Approve").
    pub kind: String,
    /// Response from the component.
    pub response: String,
}

/// Test harness for Component implementations.
///
/// Provides a minimal testing environment for components
/// without requiring OrcsEngine or EventBus.
pub struct ComponentTestHarness<C: Component> {
    /// The component under test.
    component: C,
    /// Log of requests sent to the component.
    request_log: Vec<RequestRecord>,
    /// Log of signals sent to the component.
    signal_log: Vec<SignalRecord>,
    /// Test channel ID for request context.
    test_channel: ChannelId,
}

impl<C: Component> ComponentTestHarness<C> {
    /// Creates a new test harness for the given component.
    pub fn new(component: C) -> Self {
        Self {
            component,
            request_log: Vec::new(),
            signal_log: Vec::new(),
            test_channel: ChannelId::new(),
        }
    }

    /// Returns a reference to the component under test.
    pub fn component(&self) -> &C {
        &self.component
    }

    /// Returns a mutable reference to the component under test.
    pub fn component_mut(&mut self) -> &mut C {
        &mut self.component
    }

    /// Calls `init()` on the component.
    ///
    /// # Errors
    ///
    /// Returns the component's initialization error if any.
    pub fn init(&mut self) -> Result<(), ComponentError> {
        self.component.init()
    }

    /// Sends a request to the component and logs the result.
    ///
    /// # Arguments
    ///
    /// * `request` - The request to send
    ///
    /// # Returns
    ///
    /// The result of the request.
    pub fn send_request(&mut self, request: &orcs_event::Request) -> Result<Value, ComponentError> {
        let result = self.component.on_request(request);

        self.request_log.push(RequestRecord {
            operation: request.operation.clone(),
            category: request.category.name(),
            result: match &result {
                Ok(v) => RequestResult::Ok(v.clone()),
                Err(e) => RequestResult::Err(e.to_string()),
            },
        });

        result
    }

    /// Sends a request with the given category, operation, and payload.
    ///
    /// Convenience method that builds the request internally.
    ///
    /// # Arguments
    ///
    /// * `category` - Event category
    /// * `operation` - Operation name
    /// * `payload` - Request payload
    ///
    /// # Returns
    ///
    /// The result of the request.
    pub fn request(
        &mut self,
        category: EventCategory,
        operation: &str,
        payload: Value,
    ) -> Result<Value, ComponentError> {
        let req = orcs_event::Request::new(
            category,
            operation,
            self.component.id().clone(),
            self.test_channel,
            payload,
        );
        self.send_request(&req)
    }

    /// Sends a signal to the component and logs the response.
    ///
    /// # Arguments
    ///
    /// * `signal` - The signal to send
    ///
    /// # Returns
    ///
    /// The component's response to the signal.
    pub fn send_signal(&mut self, signal: Signal) -> SignalResponse {
        let response = self.component.on_signal(&signal);

        self.signal_log.push(SignalRecord {
            kind: format!("{:?}", signal.kind),
            response: format!("{:?}", response),
        });

        response
    }

    /// Sends a Veto signal to the component.
    ///
    /// # Returns
    ///
    /// The component's response (should be `Abort`).
    pub fn veto(&mut self) -> SignalResponse {
        self.send_signal(Signal::veto(Principal::System))
    }

    /// Sends a Cancel signal for the test channel.
    ///
    /// # Returns
    ///
    /// The component's response.
    pub fn cancel(&mut self) -> SignalResponse {
        self.send_signal(Signal::cancel(self.test_channel, Principal::System))
    }

    /// Sends a Cancel signal for a specific channel.
    ///
    /// # Arguments
    ///
    /// * `channel` - The channel to cancel
    ///
    /// # Returns
    ///
    /// The component's response.
    pub fn cancel_channel(&mut self, channel: ChannelId) -> SignalResponse {
        self.send_signal(Signal::cancel(channel, Principal::System))
    }

    /// Sends an Approve signal.
    ///
    /// # Arguments
    ///
    /// * `approval_id` - The approval request ID
    ///
    /// # Returns
    ///
    /// The component's response.
    pub fn approve(&mut self, approval_id: &str) -> SignalResponse {
        self.send_signal(Signal::approve(approval_id, Principal::System))
    }

    /// Sends a Reject signal.
    ///
    /// # Arguments
    ///
    /// * `approval_id` - The approval request ID
    /// * `reason` - Optional rejection reason
    ///
    /// # Returns
    ///
    /// The component's response.
    pub fn reject(&mut self, approval_id: &str, reason: Option<String>) -> SignalResponse {
        self.send_signal(Signal::reject(approval_id, reason, Principal::System))
    }

    /// Calls `abort()` on the component.
    pub fn abort(&mut self) {
        self.component.abort();
    }

    /// Calls `shutdown()` on the component.
    pub fn shutdown(&mut self) {
        self.component.shutdown();
    }

    /// Returns the current status of the component.
    pub fn status(&self) -> Status {
        self.component.status()
    }

    /// Returns the component ID.
    pub fn id(&self) -> &ComponentId {
        self.component.id()
    }

    /// Returns the request log for snapshot testing.
    pub fn request_log(&self) -> &[RequestRecord] {
        &self.request_log
    }

    /// Returns the signal log for snapshot testing.
    pub fn signal_log(&self) -> &[SignalRecord] {
        &self.signal_log
    }

    /// Clears all logs.
    pub fn clear_logs(&mut self) {
        self.request_log.clear();
        self.signal_log.clear();
    }

    /// Returns the test channel ID.
    pub fn test_channel(&self) -> ChannelId {
        self.test_channel
    }

    /// Sets a custom test channel ID.
    pub fn with_channel(mut self, channel: ChannelId) -> Self {
        self.test_channel = channel;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Status;

    struct TestComponent {
        id: ComponentId,
        status: Status,
        request_count: usize,
    }

    impl TestComponent {
        fn new(name: &str) -> Self {
            Self {
                id: ComponentId::builtin(name),
                status: Status::Idle,
                request_count: 0,
            }
        }
    }

    impl Component for TestComponent {
        fn id(&self) -> &ComponentId {
            &self.id
        }

        fn status(&self) -> Status {
            self.status
        }

        fn on_request(&mut self, request: &orcs_event::Request) -> Result<Value, ComponentError> {
            self.request_count += 1;
            match request.operation.as_str() {
                "echo" => Ok(request.payload.clone()),
                "count" => Ok(Value::Number(self.request_count.into())),
                _ => Err(ComponentError::NotSupported(request.operation.clone())),
            }
        }

        fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
            if signal.is_veto() {
                self.abort();
                SignalResponse::Abort
            } else if matches!(signal.kind, orcs_event::SignalKind::Cancel) {
                SignalResponse::Handled
            } else {
                SignalResponse::Ignored
            }
        }

        fn abort(&mut self) {
            self.status = Status::Aborted;
        }
    }

    #[test]
    fn harness_request() {
        let comp = TestComponent::new("test");
        let mut harness = ComponentTestHarness::new(comp);

        let result = harness.request(
            EventCategory::Echo,
            "echo",
            serde_json::json!({"msg": "hello"}),
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), serde_json::json!({"msg": "hello"}));

        assert_eq!(harness.request_log().len(), 1);
        assert_eq!(harness.request_log()[0].operation, "echo");
    }

    #[test]
    fn harness_request_error() {
        let comp = TestComponent::new("test");
        let mut harness = ComponentTestHarness::new(comp);

        let result = harness.request(EventCategory::Echo, "unknown", Value::Null);
        assert!(result.is_err());

        assert_eq!(harness.request_log().len(), 1);
        assert!(matches!(
            harness.request_log()[0].result,
            RequestResult::Err(_)
        ));
    }

    #[test]
    fn harness_veto() {
        let comp = TestComponent::new("test");
        let mut harness = ComponentTestHarness::new(comp);

        assert_eq!(harness.status(), Status::Idle);

        let response = harness.veto();
        assert_eq!(response, SignalResponse::Abort);
        assert_eq!(harness.status(), Status::Aborted);

        assert_eq!(harness.signal_log().len(), 1);
        assert!(harness.signal_log()[0].kind.contains("Veto"));
    }

    #[test]
    fn harness_cancel() {
        let comp = TestComponent::new("test");
        let mut harness = ComponentTestHarness::new(comp);

        let response = harness.cancel();
        assert_eq!(response, SignalResponse::Handled);
        assert_eq!(harness.status(), Status::Idle);
    }

    #[test]
    fn harness_component_access() {
        let comp = TestComponent::new("test");
        let mut harness = ComponentTestHarness::new(comp);

        assert_eq!(harness.component().request_count, 0);

        harness
            .request(EventCategory::Echo, "count", Value::Null)
            .unwrap();
        assert_eq!(harness.component().request_count, 1);

        harness.component_mut().request_count = 100;
        assert_eq!(harness.component().request_count, 100);
    }

    #[test]
    fn harness_clear_logs() {
        let comp = TestComponent::new("test");
        let mut harness = ComponentTestHarness::new(comp);

        harness
            .request(EventCategory::Echo, "echo", Value::Null)
            .unwrap();
        harness.cancel();

        assert_eq!(harness.request_log().len(), 1);
        assert_eq!(harness.signal_log().len(), 1);

        harness.clear_logs();

        assert_eq!(harness.request_log().len(), 0);
        assert_eq!(harness.signal_log().len(), 0);
    }

    #[test]
    fn harness_init_shutdown() {
        let comp = TestComponent::new("test");
        let mut harness = ComponentTestHarness::new(comp);

        assert!(harness.init().is_ok());
        harness.shutdown();
        // Component should still be accessible after shutdown
        assert_eq!(harness.id().name, "test");
    }
}

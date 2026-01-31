//! Echo Component with HIL (Human-in-the-Loop) Support.
//!
//! A component that demonstrates EventBus-based HIL integration.
//! Before echoing user input, it requests Human approval via Signal.
//!
//! # Architecture
//!
//! This component is decoupled from HilComponent. It:
//! - Subscribes to `EventCategory::Echo`
//! - Manages its own pending approval state
//! - Receives Approve/Reject signals directly
//!
//! # Flow
//!
//! ```text
//! User                 EchoWithHilComponent              Human
//!   │                         │                            │
//!   │ Request: "echo Hi"      │                            │
//!   ├────────────────────────►│                            │
//!   │                         │ Response: pending          │
//!   │◄────────────────────────┤ (approval_id)              │
//!   │                         │                            │
//!   │                         │ (waiting for approval)     │
//!   │                         │                            │
//!   │ Signal::Approve(id)     │                            │
//!   ├────────────────────────►│                            │
//!   │                         │ Execute echo               │
//!   │       "Echo: Hi"        │                            │
//!   │◄────────────────────────┤                            │
//! ```

use orcs_component::{Component, ComponentError, EventCategory, Status};
use orcs_event::{Request, Signal, SignalKind, SignalResponse};
use orcs_types::ComponentId;
use serde_json::Value;

/// State of an echo request awaiting approval.
#[derive(Debug, Clone)]
struct PendingEcho {
    /// The approval request ID.
    approval_id: String,
    /// The original message to echo.
    message: String,
    /// Human-readable description of the approval request.
    description: String,
}

/// Echo component with HIL integration.
///
/// Requests Human approval before echoing messages.
/// Subscribes to `EventCategory::Echo` for request routing.
pub struct EchoWithHilComponent {
    id: ComponentId,
    status: Status,
    /// Pending echo request awaiting approval.
    pending: Option<PendingEcho>,
    /// Last echo result (for testing/inspection).
    last_result: Option<String>,
}

impl EchoWithHilComponent {
    /// Creates a new EchoWithHilComponent.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: ComponentId::builtin("echo_with_hil"),
            status: Status::Idle,
            pending: None,
            last_result: None,
        }
    }

    /// Returns the last echo result.
    #[must_use]
    pub fn last_result(&self) -> Option<&str> {
        self.last_result.as_deref()
    }

    /// Returns whether there's a pending approval.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Returns the pending approval ID if any.
    #[must_use]
    pub fn pending_approval_id(&self) -> Option<&str> {
        self.pending.as_ref().map(|p| p.approval_id.as_str())
    }

    /// Returns the pending approval description if any.
    #[must_use]
    pub fn pending_description(&self) -> Option<&str> {
        self.pending.as_ref().map(|p| p.description.as_str())
    }

    /// Submits an echo request for approval.
    ///
    /// Uses provided approval_id if given, otherwise generates a new one.
    fn submit_for_approval(&mut self, message: String, provided_id: Option<String>) -> String {
        let approval_id = provided_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let description = format!("You say '{}'?", message);

        self.pending = Some(PendingEcho {
            approval_id: approval_id.clone(),
            message,
            description,
        });
        self.status = Status::Running;

        approval_id
    }

    /// Executes the echo after approval.
    fn execute_echo(&mut self, message: &str) -> String {
        let result = format!("Echo: {}", message);
        self.last_result = Some(result.clone());
        self.status = Status::Idle;
        result
    }

    /// Handles an Approve signal.
    fn handle_approve(&mut self, approval_id: &str) -> bool {
        let Some(pending) = &self.pending else {
            return false;
        };

        if pending.approval_id != approval_id {
            return false;
        }

        let message = pending.message.clone();
        self.pending = None;
        self.execute_echo(&message);
        true
    }

    /// Handles a Reject signal.
    fn handle_reject(&mut self, approval_id: &str, reason: Option<&str>) -> bool {
        let Some(pending) = &self.pending else {
            return false;
        };

        if pending.approval_id != approval_id {
            return false;
        }

        self.pending = None;
        self.status = Status::Idle;
        self.last_result = Some(format!("Rejected: {}", reason.unwrap_or("No reason")));
        true
    }

    /// Handles a Modify signal.
    fn handle_modify(&mut self, approval_id: &str, modified_payload: &Value) -> bool {
        let Some(pending) = &self.pending else {
            return false;
        };

        if pending.approval_id != approval_id {
            return false;
        }

        // Use modified message if provided, otherwise use original
        let message = modified_payload
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or(&pending.message)
            .to_string();

        self.pending = None;
        self.execute_echo(&message);
        true
    }
}

impl Default for EchoWithHilComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for EchoWithHilComponent {
    fn id(&self) -> &ComponentId {
        &self.id
    }

    fn subscriptions(&self) -> Vec<EventCategory> {
        vec![EventCategory::Echo, EventCategory::Lifecycle]
    }

    fn status(&self) -> Status {
        self.status
    }

    fn on_request(&mut self, request: &Request) -> Result<Value, ComponentError> {
        match request.operation.as_str() {
            "echo" => {
                // Support both formats:
                // 1. Simple string: "hello"
                // 2. Object with message and optional approval_id: {"message": "hello", "approval_id": "xxx"}
                let (message, provided_approval_id) = if let Some(s) = request.payload.as_str() {
                    (s.to_string(), None)
                } else if let Some(obj) = request.payload.as_object() {
                    let msg = obj
                        .get("message")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            ComponentError::InvalidPayload("Expected 'message' field".into())
                        })?
                        .to_string();
                    let id = obj
                        .get("approval_id")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    (msg, id)
                } else {
                    return Err(ComponentError::InvalidPayload(
                        "Expected string or object with 'message' field".into(),
                    ));
                };

                let approval_id = self.submit_for_approval(message, provided_approval_id);

                Ok(serde_json::json!({
                    "status": "pending_approval",
                    "approval_id": approval_id,
                    "message": "Awaiting Human approval"
                }))
            }
            "check" => {
                if let Some(result) = &self.last_result {
                    Ok(serde_json::json!({
                        "status": "completed",
                        "result": result
                    }))
                } else if self.pending.is_some() {
                    Ok(serde_json::json!({
                        "status": "pending_approval"
                    }))
                } else {
                    Ok(serde_json::json!({
                        "status": "idle"
                    }))
                }
            }
            _ => Err(ComponentError::NotSupported(request.operation.clone())),
        }
    }

    fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
        match &signal.kind {
            SignalKind::Veto => {
                self.abort();
                SignalResponse::Abort
            }
            SignalKind::Approve { approval_id } => {
                if self.handle_approve(approval_id) {
                    SignalResponse::Handled
                } else {
                    SignalResponse::Ignored
                }
            }
            SignalKind::Reject {
                approval_id,
                reason,
            } => {
                if self.handle_reject(approval_id, reason.as_deref()) {
                    SignalResponse::Handled
                } else {
                    SignalResponse::Ignored
                }
            }
            SignalKind::Modify {
                approval_id,
                modified_payload,
            } => {
                if self.handle_modify(approval_id, modified_payload) {
                    SignalResponse::Handled
                } else {
                    SignalResponse::Ignored
                }
            }
            _ => SignalResponse::Ignored,
        }
    }

    fn abort(&mut self) {
        self.status = Status::Aborted;
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_types::{ChannelId, Principal, PrincipalId};

    fn test_user() -> Principal {
        Principal::User(PrincipalId::new())
    }

    fn test_request(operation: &str, payload: Value) -> Request {
        Request::new(
            EventCategory::Echo,
            operation,
            ComponentId::builtin("test"),
            ChannelId::new(),
            payload,
        )
    }

    #[test]
    fn echo_with_hil_creation() {
        let comp = EchoWithHilComponent::new();
        assert_eq!(comp.id().name, "echo_with_hil");
        assert_eq!(comp.status(), Status::Idle);
        assert!(!comp.has_pending());
        assert!(comp.last_result().is_none());
    }

    #[test]
    fn subscriptions_include_echo() {
        let comp = EchoWithHilComponent::new();
        let subs = comp.subscriptions();
        assert!(subs.contains(&EventCategory::Echo));
        assert!(subs.contains(&EventCategory::Lifecycle));
    }

    #[test]
    fn echo_request_creates_pending_approval() {
        let mut comp = EchoWithHilComponent::new();

        let req = test_request("echo", Value::String("Hi".into()));
        let result = comp.on_request(&req);

        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response["status"], "pending_approval");
        assert!(response["approval_id"].is_string());

        assert!(comp.has_pending());
        assert_eq!(comp.status(), Status::Running);
    }

    #[test]
    fn pending_has_description() {
        let mut comp = EchoWithHilComponent::new();

        let req = test_request("echo", Value::String("Hi".into()));
        comp.on_request(&req).unwrap();

        assert_eq!(comp.pending_description(), Some("You say 'Hi'?"));
    }

    #[test]
    fn echo_approved_produces_output() {
        let mut comp = EchoWithHilComponent::new();

        let req = test_request("echo", Value::String("Hi".into()));
        let result = comp.on_request(&req).unwrap();
        let approval_id = result["approval_id"].as_str().unwrap().to_string();

        assert!(comp.has_pending());
        assert!(comp.last_result().is_none());

        let signal = Signal::approve(&approval_id, test_user());
        let response = comp.on_signal(&signal);

        assert_eq!(response, SignalResponse::Handled);
        assert!(!comp.has_pending());
        assert_eq!(comp.last_result(), Some("Echo: Hi"));
        assert_eq!(comp.status(), Status::Idle);
    }

    #[test]
    fn echo_rejected_no_output() {
        let mut comp = EchoWithHilComponent::new();

        let req = test_request("echo", Value::String("Bad message".into()));
        let result = comp.on_request(&req).unwrap();
        let approval_id = result["approval_id"].as_str().unwrap().to_string();

        let signal = Signal::reject(&approval_id, Some("Not allowed".into()), test_user());
        let response = comp.on_signal(&signal);

        assert_eq!(response, SignalResponse::Handled);
        assert!(!comp.has_pending());
        assert!(comp.last_result().unwrap().starts_with("Rejected:"));
        assert_eq!(comp.status(), Status::Idle);
    }

    #[test]
    fn echo_modified_uses_new_message() {
        let mut comp = EchoWithHilComponent::new();

        let req = test_request("echo", Value::String("Original".into()));
        let result = comp.on_request(&req).unwrap();
        let approval_id = result["approval_id"].as_str().unwrap().to_string();

        let modified_payload = serde_json::json!({ "message": "Modified" });
        let signal = Signal::modify(&approval_id, modified_payload, test_user());
        let response = comp.on_signal(&signal);

        assert_eq!(response, SignalResponse::Handled);
        assert!(!comp.has_pending());
        assert_eq!(comp.last_result(), Some("Echo: Modified"));
    }

    #[test]
    fn echo_veto_aborts() {
        let mut comp = EchoWithHilComponent::new();

        let req = test_request("echo", Value::String("Hi".into()));
        let _ = comp.on_request(&req);

        let signal = Signal::veto(test_user());
        let response = comp.on_signal(&signal);

        assert_eq!(response, SignalResponse::Abort);
        assert_eq!(comp.status(), Status::Aborted);
        assert!(!comp.has_pending());
    }

    #[test]
    fn echo_check_status() {
        let mut comp = EchoWithHilComponent::new();

        // Check idle state
        let req = test_request("check", Value::Null);
        let result = comp.on_request(&req).unwrap();
        assert_eq!(result["status"], "idle");

        // Start echo
        let echo_req = test_request("echo", Value::String("Hi".into()));
        let echo_result = comp.on_request(&echo_req).unwrap();
        let approval_id = echo_result["approval_id"].as_str().unwrap().to_string();

        // Check pending state
        let result = comp.on_request(&req).unwrap();
        assert_eq!(result["status"], "pending_approval");

        // Approve
        let signal = Signal::approve(&approval_id, test_user());
        comp.on_signal(&signal);

        // Check completed state
        let result = comp.on_request(&req).unwrap();
        assert_eq!(result["status"], "completed");
        assert_eq!(result["result"], "Echo: Hi");
    }

    #[test]
    fn echo_invalid_payload() {
        let mut comp = EchoWithHilComponent::new();

        let req = test_request("echo", serde_json::json!({"not": "string"}));
        let result = comp.on_request(&req);

        assert!(result.is_err());
        match result.unwrap_err() {
            ComponentError::InvalidPayload(_) => {}
            _ => panic!("Expected InvalidPayload error"),
        }
    }

    #[test]
    fn echo_unsupported_operation() {
        let mut comp = EchoWithHilComponent::new();

        let req = test_request("unknown", Value::Null);
        let result = comp.on_request(&req);

        assert!(result.is_err());
        match result.unwrap_err() {
            ComponentError::NotSupported(op) => assert_eq!(op, "unknown"),
            _ => panic!("Expected NotSupported error"),
        }
    }

    #[test]
    fn signal_wrong_approval_id_ignored() {
        let mut comp = EchoWithHilComponent::new();

        let req = test_request("echo", Value::String("Hi".into()));
        let _ = comp.on_request(&req);

        // Wrong approval_id
        let signal = Signal::approve("wrong-id", test_user());
        let response = comp.on_signal(&signal);

        assert_eq!(response, SignalResponse::Ignored);
        assert!(comp.has_pending()); // Still pending
    }

    #[test]
    fn signal_no_pending_ignored() {
        let mut comp = EchoWithHilComponent::new();

        // No pending request
        let signal = Signal::approve("any-id", test_user());
        let response = comp.on_signal(&signal);

        assert_eq!(response, SignalResponse::Ignored);
    }

    /// E2E test: Full user interaction flow
    #[test]
    fn e2e_user_says_hi_and_approves() {
        let mut comp = EchoWithHilComponent::new();

        // User: "Hi"
        let req = test_request("echo", Value::String("Hi".into()));
        let result = comp.on_request(&req).unwrap();

        // System prompts: "You say 'Hi'?"
        assert_eq!(result["status"], "pending_approval");
        let approval_id = result["approval_id"].as_str().unwrap();

        // Verify the approval description
        assert_eq!(comp.pending_description(), Some("You say 'Hi'?"));

        // User: "y" (approve)
        let signal = Signal::approve(approval_id, test_user());
        comp.on_signal(&signal);

        // System: "Echo: Hi"
        assert_eq!(comp.last_result(), Some("Echo: Hi"));
    }

    /// E2E test: User rejects
    #[test]
    fn e2e_user_says_bad_word_and_rejects() {
        let mut comp = EchoWithHilComponent::new();

        // User: "BadWord"
        let req = test_request("echo", Value::String("BadWord".into()));
        let result = comp.on_request(&req).unwrap();
        let approval_id = result["approval_id"].as_str().unwrap();

        // System prompts: "You say 'BadWord'?"
        assert_eq!(comp.pending_description(), Some("You say 'BadWord'?"));

        // User: "n" (reject)
        let signal = Signal::reject(approval_id, Some("Changed my mind".into()), test_user());
        comp.on_signal(&signal);

        // No echo output
        assert!(comp.last_result().unwrap().contains("Rejected"));
    }
}

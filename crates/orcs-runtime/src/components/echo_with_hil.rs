//! Echo with HIL Component.
//!
//! A test component that demonstrates HIL integration.
//! Before echoing user input, it requests Human approval.
//!
//! # Flow
//!
//! ```text
//! User                 EchoWithHilComponent           HilComponent
//!   │                         │                            │
//!   │ "echo Hi"               │                            │
//!   ├────────────────────────►│                            │
//!   │                         │ ApprovalRequest            │
//!   │                         │ "You say 'Hi'?"            │
//!   │                         ├───────────────────────────►│
//!   │                         │                            │
//!   │ Signal::Approve         │                            │
//!   ├─────────────────────────┼───────────────────────────►│
//!   │                         │        Approved            │
//!   │                         │◄───────────────────────────┤
//!   │       "Echo: Hi"        │                            │
//!   │◄────────────────────────┤                            │
//! ```

use crate::components::{ApprovalRequest, ApprovalResult, HilComponent};
use orcs_component::{Component, ComponentError, Status};
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
}

/// Echo component with HIL integration.
///
/// Requests Human approval before echoing messages.
pub struct EchoWithHilComponent {
    id: ComponentId,
    status: Status,
    /// Internal HIL component for approval management.
    hil: HilComponent,
    /// Pending echo requests awaiting approval.
    pending: Option<PendingEcho>,
    /// Last echo result (for testing).
    last_result: Option<String>,
}

impl EchoWithHilComponent {
    /// Creates a new EchoWithHilComponent.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: ComponentId::builtin("echo_with_hil"),
            status: Status::Idle,
            hil: HilComponent::new(),
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

    /// Submits an echo request for approval.
    fn submit_for_approval(&mut self, message: String) -> String {
        let request = ApprovalRequest::new(
            "echo",
            format!("You say '{}'?", message),
            serde_json::json!({ "message": message }),
        );
        let approval_id = self.hil.submit(request);

        self.pending = Some(PendingEcho {
            approval_id: approval_id.clone(),
            message,
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

    /// Handles approval result from HIL.
    fn handle_approval(&mut self, approval_id: &str, result: &ApprovalResult) -> Option<String> {
        let pending = self.pending.take()?;

        if pending.approval_id != approval_id {
            // Not our approval, restore pending
            self.pending = Some(pending);
            return None;
        }

        match result {
            ApprovalResult::Approved => Some(self.execute_echo(&pending.message)),
            ApprovalResult::Modified { modified_payload } => {
                // Use modified message if provided
                let message = modified_payload
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&pending.message);
                Some(self.execute_echo(message))
            }
            ApprovalResult::Rejected { reason } => {
                self.status = Status::Idle;
                self.last_result = Some(format!(
                    "Rejected: {}",
                    reason.as_deref().unwrap_or("No reason")
                ));
                None
            }
        }
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

    fn status(&self) -> Status {
        self.status
    }

    fn on_request(&mut self, request: &Request) -> Result<Value, ComponentError> {
        match request.operation.as_str() {
            "echo" => {
                let message = request
                    .payload
                    .as_str()
                    .ok_or_else(|| {
                        ComponentError::InvalidPayload("Expected string message".into())
                    })?
                    .to_string();

                let approval_id = self.submit_for_approval(message);

                Ok(serde_json::json!({
                    "status": "pending_approval",
                    "approval_id": approval_id,
                    "message": "Awaiting Human approval"
                }))
            }
            "check" => {
                // Check if there's a result ready
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
        // First, let HIL handle the signal
        let hil_response = self.hil.on_signal(signal);

        match hil_response {
            SignalResponse::Handled => {
                // HIL handled it - check if our pending request was resolved
                if let Some(pending) = &self.pending {
                    // Check if the approval was resolved
                    if !self.hil.is_pending(&pending.approval_id) {
                        // Infer result from signal type
                        let approval_id = pending.approval_id.clone();
                        if signal.is_approve() {
                            let _ = self.handle_approval(&approval_id, &ApprovalResult::Approved);
                        } else if signal.is_reject() {
                            let _ = self.handle_approval(
                                &approval_id,
                                &ApprovalResult::Rejected { reason: None },
                            );
                        } else if let SignalKind::Modify {
                            modified_payload, ..
                        } = &signal.kind
                        {
                            let _ = self.handle_approval(
                                &approval_id,
                                &ApprovalResult::Modified {
                                    modified_payload: modified_payload.clone(),
                                },
                            );
                        }
                    }
                }
                SignalResponse::Handled
            }
            SignalResponse::Abort => {
                self.abort();
                SignalResponse::Abort
            }
            SignalResponse::Ignored => {
                // Check for veto
                if signal.is_veto() {
                    self.abort();
                    SignalResponse::Abort
                } else {
                    SignalResponse::Ignored
                }
            }
        }
    }

    fn abort(&mut self) {
        self.status = Status::Aborted;
        self.pending = None;
        self.hil.abort();
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
    fn echo_approved_produces_output() {
        let mut comp = EchoWithHilComponent::new();

        // Step 1: User sends "Hi"
        let req = test_request("echo", Value::String("Hi".into()));
        let result = comp.on_request(&req).unwrap();
        let approval_id = result["approval_id"].as_str().unwrap().to_string();

        assert!(comp.has_pending());
        assert!(comp.last_result().is_none());

        // Step 2: User approves
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

        // Step 1: User sends "Bad message"
        let req = test_request("echo", Value::String("Bad message".into()));
        let result = comp.on_request(&req).unwrap();
        let approval_id = result["approval_id"].as_str().unwrap().to_string();

        // Step 2: User rejects
        let signal = Signal::reject(&approval_id, Some("Not allowed".into()), test_user());
        let response = comp.on_signal(&signal);

        assert_eq!(response, SignalResponse::Handled);
        assert!(!comp.has_pending());
        // Rejected doesn't produce echo output
        assert!(comp.last_result().unwrap().starts_with("Rejected:"));
        assert_eq!(comp.status(), Status::Idle);
    }

    #[test]
    fn echo_modified_uses_new_message() {
        let mut comp = EchoWithHilComponent::new();

        // Step 1: User sends "Original"
        let req = test_request("echo", Value::String("Original".into()));
        let result = comp.on_request(&req).unwrap();
        let approval_id = result["approval_id"].as_str().unwrap().to_string();

        // Step 2: User modifies
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

        // Start an echo request
        let req = test_request("echo", Value::String("Hi".into()));
        let _ = comp.on_request(&req);

        // Veto
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

        // Send non-string payload
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

        // Verify the approval request description
        let pending_req = comp.hil.get_pending(approval_id).unwrap();
        assert_eq!(pending_req.description, "You say 'Hi'?");

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
        let pending_req = comp.hil.get_pending(approval_id).unwrap();
        assert_eq!(pending_req.description, "You say 'BadWord'?");

        // User: "n" (reject)
        let signal = Signal::reject(approval_id, Some("Changed my mind".into()), test_user());
        comp.on_signal(&signal);

        // No echo output
        assert!(comp.last_result().unwrap().contains("Rejected"));
    }
}

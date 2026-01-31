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

use orcs_component::{
    Component, ComponentError, EventCategory, Package, PackageError, PackageInfo, Packageable,
    Status,
};
use orcs_event::{Request, Signal, SignalKind, SignalResponse};
use orcs_types::ComponentId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Decorator package configuration.
///
/// Modifies echo output with prefix and suffix.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DecoratorConfig {
    /// Prefix to add before the echo message.
    pub prefix: String,
    /// Suffix to add after the echo message.
    pub suffix: String,
}

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
///
/// Supports packages for customizing output (e.g., decorators).
pub struct EchoWithHilComponent {
    id: ComponentId,
    status: Status,
    /// Pending echo request awaiting approval.
    pending: Option<PendingEcho>,
    /// Last echo result (for testing/inspection).
    last_result: Option<String>,
    /// Installed packages.
    installed_packages: Vec<PackageInfo>,
    /// Current decorator configuration.
    decorator: DecoratorConfig,
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
            installed_packages: Vec::new(),
            decorator: DecoratorConfig::default(),
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
    ///
    /// Applies decorator prefix/suffix if configured.
    fn execute_echo(&mut self, message: &str) -> String {
        let decorated = format!(
            "{}{}{}",
            self.decorator.prefix, message, self.decorator.suffix
        );
        let result = format!("Echo: {}", decorated);
        self.last_result = Some(result.clone());
        self.status = Status::Idle;
        result
    }

    /// Returns the current decorator configuration.
    #[must_use]
    pub fn decorator(&self) -> &DecoratorConfig {
        &self.decorator
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

    fn subscriptions(&self) -> &[EventCategory] {
        &[EventCategory::Echo, EventCategory::Lifecycle]
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

    fn as_packageable(&self) -> Option<&dyn Packageable> {
        Some(self)
    }

    fn as_packageable_mut(&mut self) -> Option<&mut dyn Packageable> {
        Some(self)
    }
}

impl Packageable for EchoWithHilComponent {
    fn list_packages(&self) -> &[PackageInfo] {
        &self.installed_packages
    }

    fn install_package(&mut self, package: &Package) -> Result<(), PackageError> {
        // Check if already installed
        if self.is_installed(package.id()) {
            return Err(PackageError::AlreadyInstalled(package.id().to_string()));
        }

        // Parse decorator config from package content
        let config: DecoratorConfig = package.to_content()?;

        // Apply decorator
        self.decorator = config;
        self.installed_packages.push(package.info.clone());

        tracing::info!(
            "Installed package '{}': prefix='{}', suffix='{}'",
            package.id(),
            self.decorator.prefix,
            self.decorator.suffix
        );

        Ok(())
    }

    fn uninstall_package(&mut self, package_id: &str) -> Result<(), PackageError> {
        // Check if installed
        if !self.is_installed(package_id) {
            return Err(PackageError::NotFound(package_id.to_string()));
        }

        // Remove from list
        self.installed_packages.retain(|p| p.id != package_id);

        // Reset decorator to default
        self.decorator = DecoratorConfig::default();

        tracing::info!("Uninstalled package '{}'", package_id);

        Ok(())
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

    // --- Package tests ---

    fn create_decorator_package(id: &str, prefix: &str, suffix: &str) -> Package {
        let info = PackageInfo::new(id, "Decorator", "1.0.0", "Add decoration to echo output");
        let config = DecoratorConfig {
            prefix: prefix.to_string(),
            suffix: suffix.to_string(),
        };
        Package::new(info, &config).unwrap()
    }

    #[test]
    fn package_install_decorator() {
        let mut comp = EchoWithHilComponent::new();

        let package = create_decorator_package("angle-brackets", "<<", ">>");
        comp.install_package(&package).unwrap();

        assert!(comp.is_installed("angle-brackets"));
        assert_eq!(comp.decorator().prefix, "<<");
        assert_eq!(comp.decorator().suffix, ">>");
        assert_eq!(comp.list_packages().len(), 1);
    }

    #[test]
    fn package_uninstall_decorator() {
        let mut comp = EchoWithHilComponent::new();

        let package = create_decorator_package("stars", "***", "***");
        comp.install_package(&package).unwrap();
        comp.uninstall_package("stars").unwrap();

        assert!(!comp.is_installed("stars"));
        assert_eq!(comp.decorator().prefix, "");
        assert_eq!(comp.decorator().suffix, "");
        assert!(comp.list_packages().is_empty());
    }

    #[test]
    fn package_already_installed_error() {
        let mut comp = EchoWithHilComponent::new();

        let package = create_decorator_package("test-pkg", "[", "]");
        comp.install_package(&package).unwrap();

        let result = comp.install_package(&package);
        assert!(matches!(result, Err(PackageError::AlreadyInstalled(_))));
    }

    #[test]
    fn package_not_found_error() {
        let mut comp = EchoWithHilComponent::new();

        let result = comp.uninstall_package("nonexistent");
        assert!(matches!(result, Err(PackageError::NotFound(_))));
    }

    #[test]
    fn package_decorator_applied_to_echo() {
        let mut comp = EchoWithHilComponent::new();

        // Install decorator
        let package = create_decorator_package("brackets", "[", "]");
        comp.install_package(&package).unwrap();

        // Request echo
        let req = test_request("echo", Value::String("Hello".into()));
        let result = comp.on_request(&req).unwrap();
        let approval_id = result["approval_id"].as_str().unwrap();

        // Approve
        let signal = Signal::approve(approval_id, test_user());
        comp.on_signal(&signal);

        // Verify decorated output
        assert_eq!(comp.last_result(), Some("Echo: [Hello]"));
    }

    #[test]
    fn package_uninstall_removes_decoration() {
        let mut comp = EchoWithHilComponent::new();

        // Install and then uninstall
        let package = create_decorator_package("brackets", "[", "]");
        comp.install_package(&package).unwrap();
        comp.uninstall_package("brackets").unwrap();

        // Request echo
        let req = test_request("echo", Value::String("Hello".into()));
        let result = comp.on_request(&req).unwrap();
        let approval_id = result["approval_id"].as_str().unwrap();

        // Approve
        let signal = Signal::approve(approval_id, test_user());
        comp.on_signal(&signal);

        // Verify no decoration
        assert_eq!(comp.last_result(), Some("Echo: Hello"));
    }
}

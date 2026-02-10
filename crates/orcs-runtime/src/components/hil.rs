//! Human-in-the-Loop (HIL) Component.
//!
//! Manages approval requests that require Human confirmation before execution.
//!
//! # Overview
//!
//! The HIL component sits between tool execution and the actual operation,
//! ensuring Human approval for potentially dangerous or irreversible actions.
//!
//! ```text
//! ToolsComponent                 HilComponent              Human
//!       │                             │                      │
//!       │ Request: need approval      │                      │
//!       ├────────────────────────────►│                      │
//!       │                             │ Display approval     │
//!       │                             │ request              │
//!       │                             ├─────────────────────►│
//!       │                             │                      │
//!       │                             │     Approve/Reject   │
//!       │                             │◄─────────────────────┤
//!       │    Response: approved/      │                      │
//!       │◄────────────────────────────┤                      │
//! ```
//!
//! # Example
//!
//! ```
//! use orcs_runtime::components::{HilComponent, ApprovalRequest};
//! use orcs_types::ComponentId;
//!
//! let mut hil = HilComponent::new();
//!
//! // Request approval for a file write operation
//! let request = ApprovalRequest::new(
//!     "write",
//!     "Write to /etc/hosts",
//!     serde_json::json!({ "path": "/etc/hosts" }),
//! );
//!
//! let id = hil.submit(request);
//! assert!(hil.is_pending(&id));
//! ```

use orcs_component::{
    Component, ComponentError, ComponentSnapshot, EventCategory, SnapshotError, Status,
};
use orcs_event::{Request, Signal, SignalKind, SignalResponse};
use orcs_types::ComponentId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Request for Human approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// Unique ID for this approval request.
    pub id: String,
    /// Operation type (e.g., "write", "bash", "delete").
    pub operation: String,
    /// Human-readable description of what will happen.
    pub description: String,
    /// Additional context/payload.
    pub context: Value,
    /// Timestamp when the request was created.
    pub created_at_ms: u64,
}

impl ApprovalRequest {
    /// Creates a new approval request.
    ///
    /// The ID is automatically generated.
    #[must_use]
    pub fn new(
        operation: impl Into<String>,
        description: impl Into<String>,
        context: Value,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            operation: operation.into(),
            description: description.into(),
            context,
            // Note: as_millis() returns u128, but realistically timestamps
            // won't exceed u64::MAX until year 584 million. We saturate to
            // u64::MAX as a safe fallback for theoretical overflow.
            created_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                .unwrap_or(0),
        }
    }

    /// Creates an approval request with a specific ID.
    #[must_use]
    pub fn with_id(
        id: impl Into<String>,
        operation: impl Into<String>,
        description: impl Into<String>,
        context: Value,
    ) -> Self {
        Self {
            id: id.into(),
            operation: operation.into(),
            description: description.into(),
            context,
            // See note in `new()` about timestamp handling
            created_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                .unwrap_or(0),
        }
    }
}

/// Result of an approval request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ApprovalResult {
    /// Request was approved by Human.
    Approved,
    /// Request was rejected by Human.
    Rejected {
        /// Optional reason for rejection.
        reason: Option<String>,
    },
    /// Request was approved with modifications by Human.
    Modified {
        /// The modified payload to use instead of original.
        modified_payload: Value,
    },
}

impl ApprovalResult {
    /// Returns `true` if the result is `Approved` or `Modified`.
    #[must_use]
    pub fn is_approved(&self) -> bool {
        matches!(self, Self::Approved | Self::Modified { .. })
    }

    /// Returns `true` if the result is `Rejected`.
    #[must_use]
    pub fn is_rejected(&self) -> bool {
        matches!(self, Self::Rejected { .. })
    }

    /// Returns `true` if the result is `Modified`.
    #[must_use]
    pub fn is_modified(&self) -> bool {
        matches!(self, Self::Modified { .. })
    }

    /// Returns the modified payload if this is a `Modified` result.
    #[must_use]
    pub fn modified_payload(&self) -> Option<&Value> {
        match self {
            Self::Modified { modified_payload } => Some(modified_payload),
            _ => None,
        }
    }
}

/// Human-in-the-Loop Component.
///
/// Manages a queue of approval requests and responds to Human
/// Approve/Reject signals.
pub struct HilComponent {
    id: ComponentId,
    status: Status,
    /// Pending approval requests by ID.
    pending: HashMap<String, ApprovalRequest>,
    /// Resolved requests (for audit trail).
    resolved: HashMap<String, (ApprovalRequest, ApprovalResult)>,
}

impl HilComponent {
    /// Creates a new HIL component.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: ComponentId::builtin("hil"),
            status: Status::Idle,
            pending: HashMap::new(),
            resolved: HashMap::new(),
        }
    }

    /// Submits a new approval request.
    ///
    /// Returns the request ID that can be used to check status or cancel.
    pub fn submit(&mut self, request: ApprovalRequest) -> String {
        let id = request.id.clone();
        self.pending.insert(id.clone(), request);
        id
    }

    /// Returns `true` if there are pending approval requests.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Returns `true` if the given request ID is pending.
    #[must_use]
    pub fn is_pending(&self, id: &str) -> bool {
        self.pending.contains_key(id)
    }

    /// Returns a reference to a pending request.
    #[must_use]
    pub fn get_pending(&self, id: &str) -> Option<&ApprovalRequest> {
        self.pending.get(id)
    }

    /// Returns all pending requests.
    #[must_use]
    pub fn pending_requests(&self) -> Vec<&ApprovalRequest> {
        self.pending.values().collect()
    }

    /// Returns the number of pending requests.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Returns a resolved request by ID.
    #[must_use]
    pub fn get_resolved(&self, id: &str) -> Option<&(ApprovalRequest, ApprovalResult)> {
        self.resolved.get(id)
    }

    /// Returns the number of resolved requests.
    #[must_use]
    pub fn resolved_count(&self) -> usize {
        self.resolved.len()
    }

    /// Resolves an approval request.
    ///
    /// Returns `Ok(result)` if the request was found and resolved,
    /// or `Err` if the request ID was not found.
    pub fn resolve(
        &mut self,
        id: &str,
        result: ApprovalResult,
    ) -> Result<ApprovalResult, ComponentError> {
        if let Some(request) = self.pending.remove(id) {
            self.resolved
                .insert(id.to_string(), (request, result.clone()));
            Ok(result)
        } else {
            Err(ComponentError::ExecutionFailed(format!(
                "Approval request not found: {}",
                id
            )))
        }
    }

    /// Handles an Approve signal.
    fn handle_approve(&mut self, approval_id: &str) -> SignalResponse {
        match self.resolve(approval_id, ApprovalResult::Approved) {
            Ok(_) => SignalResponse::Handled,
            Err(_) => SignalResponse::Ignored,
        }
    }

    /// Handles a Reject signal.
    fn handle_reject(&mut self, approval_id: &str, reason: Option<String>) -> SignalResponse {
        match self.resolve(approval_id, ApprovalResult::Rejected { reason }) {
            Ok(_) => SignalResponse::Handled,
            Err(_) => SignalResponse::Ignored,
        }
    }

    /// Handles a Modify signal.
    fn handle_modify(&mut self, approval_id: &str, modified_payload: Value) -> SignalResponse {
        match self.resolve(approval_id, ApprovalResult::Modified { modified_payload }) {
            Ok(_) => SignalResponse::Handled,
            Err(_) => SignalResponse::Ignored,
        }
    }
}

impl Default for HilComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for HilComponent {
    fn id(&self) -> &ComponentId {
        &self.id
    }

    fn subscriptions(&self) -> &[EventCategory] {
        &[EventCategory::Hil, EventCategory::Lifecycle]
    }

    fn status(&self) -> Status {
        self.status
    }

    fn on_request(&mut self, request: &Request) -> Result<Value, ComponentError> {
        match request.operation.as_str() {
            "submit" => {
                // Parse the approval request from payload
                let approval_req: ApprovalRequest = serde_json::from_value(request.payload.clone())
                    .map_err(|e| {
                        ComponentError::InvalidPayload(format!("Invalid approval request: {}", e))
                    })?;

                let id = self.submit(approval_req);
                Ok(serde_json::json!({ "approval_id": id, "status": "pending" }))
            }
            "status" => {
                // Check status of a specific request
                let id = request
                    .payload
                    .get("approval_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ComponentError::InvalidPayload("Missing approval_id".into()))?;

                if self.is_pending(id) {
                    Ok(serde_json::json!({ "status": "pending" }))
                } else if let Some((_, result)) = self.resolved.get(id) {
                    Ok(serde_json::json!({ "status": "resolved", "result": result }))
                } else {
                    Err(ComponentError::ExecutionFailed(format!(
                        "Approval request not found: {}",
                        id
                    )))
                }
            }
            "list" => {
                // List all pending requests
                let pending: Vec<_> = self.pending_requests().into_iter().cloned().collect();
                Ok(serde_json::json!({ "pending": pending }))
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
            SignalKind::Approve { approval_id } => self.handle_approve(approval_id),
            SignalKind::Reject {
                approval_id,
                reason,
            } => self.handle_reject(approval_id, reason.clone()),
            SignalKind::Modify {
                approval_id,
                modified_payload,
            } => self.handle_modify(approval_id, modified_payload.clone()),
            _ => SignalResponse::Ignored,
        }
    }

    fn abort(&mut self) {
        self.status = Status::Aborted;
        // Reject all pending requests on abort
        let pending_ids: Vec<_> = self.pending.keys().cloned().collect();
        for id in pending_ids {
            let _ = self.resolve(
                &id,
                ApprovalResult::Rejected {
                    reason: Some("Aborted".into()),
                },
            );
        }
    }

    fn snapshot(&self) -> Result<ComponentSnapshot, SnapshotError> {
        let state = HilSnapshot::from_component(self);
        ComponentSnapshot::from_state(self.id.fqn(), &state)
    }

    fn restore(&mut self, snapshot: &ComponentSnapshot) -> Result<(), SnapshotError> {
        snapshot.validate(&self.id.fqn())?;
        let state: HilSnapshot = snapshot.to_state()?;
        state.apply_to(self);
        Ok(())
    }
}

/// Serializable snapshot of HilComponent state.
///
/// Only `resolved` entries are persisted (audit trail).
/// `pending` entries are transient and not meaningful across sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HilSnapshot {
    /// Resolved approval records (request + result pairs).
    resolved: Vec<ResolvedRecord>,
}

/// A single resolved approval record for serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResolvedRecord {
    id: String,
    request: ApprovalRequest,
    result: ApprovalResult,
}

impl HilSnapshot {
    fn from_component(hil: &HilComponent) -> Self {
        let resolved = hil
            .resolved
            .iter()
            .map(|(id, (req, result))| ResolvedRecord {
                id: id.clone(),
                request: req.clone(),
                result: result.clone(),
            })
            .collect();
        Self { resolved }
    }

    fn apply_to(&self, hil: &mut HilComponent) {
        for record in &self.resolved {
            hil.resolved.insert(
                record.id.clone(),
                (record.request.clone(), record.result.clone()),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_component::EventCategory;
    use orcs_types::{ChannelId, Principal, PrincipalId};

    fn test_user() -> Principal {
        Principal::User(PrincipalId::new())
    }

    #[test]
    fn hil_component_creation() {
        let hil = HilComponent::new();
        assert_eq!(hil.id().name, "hil");
        assert_eq!(hil.status(), Status::Idle);
        assert!(!hil.has_pending());
    }

    #[test]
    fn hil_submit_request() {
        let mut hil = HilComponent::new();

        let req = ApprovalRequest::new("write", "Write to file.txt", serde_json::json!({}));
        let id = hil.submit(req);

        assert!(hil.is_pending(&id));
        assert!(hil.has_pending());
        assert_eq!(hil.pending_count(), 1);
    }

    #[test]
    fn hil_resolve_approve() {
        let mut hil = HilComponent::new();

        let req = ApprovalRequest::with_id(
            "req-123",
            "write",
            "Write to file.txt",
            serde_json::json!({}),
        );
        hil.submit(req);

        let result = hil
            .resolve("req-123", ApprovalResult::Approved)
            .expect("resolve approve");
        assert!(result.is_approved());
        assert!(!hil.is_pending("req-123"));
    }

    #[test]
    fn hil_resolve_reject() {
        let mut hil = HilComponent::new();

        let req = ApprovalRequest::with_id("req-456", "bash", "Run rm -rf", serde_json::json!({}));
        hil.submit(req);

        let result = hil.resolve(
            "req-456",
            ApprovalResult::Rejected {
                reason: Some("Too dangerous".into()),
            },
        );
        let result = result.expect("resolve reject");
        assert!(result.is_rejected());
    }

    #[test]
    fn hil_resolve_not_found() {
        let mut hil = HilComponent::new();

        let result = hil.resolve("nonexistent", ApprovalResult::Approved);
        assert!(result.is_err());
    }

    #[test]
    fn hil_signal_approve() {
        let mut hil = HilComponent::new();

        let req = ApprovalRequest::with_id("req-789", "write", "Write file", serde_json::json!({}));
        hil.submit(req);

        let signal = Signal::approve("req-789", test_user());
        let response = hil.on_signal(&signal);

        assert_eq!(response, SignalResponse::Handled);
        assert!(!hil.is_pending("req-789"));
    }

    #[test]
    fn hil_signal_reject() {
        let mut hil = HilComponent::new();

        let req = ApprovalRequest::with_id("req-abc", "bash", "Run command", serde_json::json!({}));
        hil.submit(req);

        let signal = Signal::reject("req-abc", Some("Not allowed".into()), test_user());
        let response = hil.on_signal(&signal);

        assert_eq!(response, SignalResponse::Handled);
        assert!(!hil.is_pending("req-abc"));
    }

    #[test]
    fn hil_signal_approve_not_found() {
        let mut hil = HilComponent::new();

        let signal = Signal::approve("nonexistent", test_user());
        let response = hil.on_signal(&signal);

        assert_eq!(response, SignalResponse::Ignored);
    }

    #[test]
    fn hil_abort_rejects_all_pending() {
        let mut hil = HilComponent::new();

        hil.submit(ApprovalRequest::with_id(
            "req-1",
            "op1",
            "desc1",
            serde_json::json!({}),
        ));
        hil.submit(ApprovalRequest::with_id(
            "req-2",
            "op2",
            "desc2",
            serde_json::json!({}),
        ));
        assert_eq!(hil.pending_count(), 2);

        hil.abort();

        assert_eq!(hil.status(), Status::Aborted);
        assert_eq!(hil.pending_count(), 0);
    }

    #[test]
    fn hil_request_submit() {
        let mut hil = HilComponent::new();
        let source = ComponentId::builtin("tools");
        let channel = ChannelId::new();

        let payload = serde_json::json!({
            "id": "custom-id",
            "operation": "write",
            "description": "Write to file",
            "context": {},
            "created_at_ms": 0
        });

        let req = Request::new(EventCategory::Hil, "submit", source, channel, payload);
        let result = hil.on_request(&req);

        let response = result.expect("submit request");
        assert_eq!(response["status"], "pending");
        assert!(hil.is_pending("custom-id"));
    }

    #[test]
    fn hil_request_list() {
        let mut hil = HilComponent::new();
        hil.submit(ApprovalRequest::with_id(
            "req-1",
            "op1",
            "desc1",
            serde_json::json!({}),
        ));
        hil.submit(ApprovalRequest::with_id(
            "req-2",
            "op2",
            "desc2",
            serde_json::json!({}),
        ));

        let source = ComponentId::builtin("test");
        let channel = ChannelId::new();
        let req = Request::new(
            EventCategory::Hil,
            "list",
            source,
            channel,
            serde_json::json!({}),
        );

        let response = hil.on_request(&req).expect("list request");
        let pending = response["pending"]
            .as_array()
            .expect("pending should be array");
        assert_eq!(pending.len(), 2);
    }

    #[test]
    fn hil_request_status() {
        let mut hil = HilComponent::new();
        hil.submit(ApprovalRequest::with_id(
            "check-me",
            "op",
            "desc",
            serde_json::json!({}),
        ));

        let source = ComponentId::builtin("test");
        let channel = ChannelId::new();
        let req = Request::new(
            EventCategory::Hil,
            "status",
            source,
            channel,
            serde_json::json!({ "approval_id": "check-me" }),
        );

        let response = hil.on_request(&req).expect("status request");
        assert_eq!(response["status"], "pending");
    }

    #[test]
    fn hil_signal_modify() {
        let mut hil = HilComponent::new();

        let req = ApprovalRequest::with_id("req-mod", "write", "Write file", serde_json::json!({}));
        hil.submit(req);

        let modified_payload = serde_json::json!({
            "path": "/safe/path/file.txt",
            "content": "modified content"
        });
        let signal = Signal::modify("req-mod", modified_payload.clone(), test_user());
        let response = hil.on_signal(&signal);

        assert_eq!(response, SignalResponse::Handled);
        assert!(!hil.is_pending("req-mod"));

        // Check resolved result
        let (_, result) = hil.get_resolved("req-mod").expect("resolved req-mod");
        assert!(result.is_modified());
        assert!(result.is_approved()); // Modified counts as approved
        assert_eq!(result.modified_payload(), Some(&modified_payload));
    }

    #[test]
    fn hil_signal_modify_not_found() {
        let mut hil = HilComponent::new();

        let signal = Signal::modify("nonexistent", serde_json::json!({}), test_user());
        let response = hil.on_signal(&signal);

        assert_eq!(response, SignalResponse::Ignored);
    }

    #[test]
    fn approval_result_helpers() {
        let approved = ApprovalResult::Approved;
        assert!(approved.is_approved());
        assert!(!approved.is_rejected());
        assert!(!approved.is_modified());
        assert!(approved.modified_payload().is_none());

        let rejected = ApprovalResult::Rejected {
            reason: Some("test".into()),
        };
        assert!(!rejected.is_approved());
        assert!(rejected.is_rejected());
        assert!(!rejected.is_modified());

        let modified = ApprovalResult::Modified {
            modified_payload: serde_json::json!({"key": "value"}),
        };
        assert!(modified.is_approved()); // Modified counts as approved
        assert!(!modified.is_rejected());
        assert!(modified.is_modified());
        assert!(modified.modified_payload().is_some());
    }

    // === Snapshot tests ===

    #[test]
    fn hil_snapshot_empty() {
        let hil = HilComponent::new();
        let snapshot = hil.snapshot().expect("snapshot empty hil");

        assert_eq!(snapshot.component_fqn, hil.id().fqn());
        assert!(!snapshot.is_empty());

        let state: HilSnapshot = snapshot.to_state().expect("deserialize snapshot");
        assert!(state.resolved.is_empty());
    }

    #[test]
    fn hil_snapshot_with_resolved() {
        let mut hil = HilComponent::new();

        // Submit and resolve some requests
        hil.submit(ApprovalRequest::with_id(
            "req-1",
            "write",
            "Write file",
            serde_json::json!({}),
        ));
        hil.resolve("req-1", ApprovalResult::Approved)
            .expect("resolve req-1");

        hil.submit(ApprovalRequest::with_id(
            "req-2",
            "bash",
            "Run command",
            serde_json::json!({}),
        ));
        hil.resolve(
            "req-2",
            ApprovalResult::Rejected {
                reason: Some("dangerous".into()),
            },
        )
        .expect("resolve req-2");

        let snapshot = hil.snapshot().expect("snapshot with resolved");
        let state: HilSnapshot = snapshot.to_state().expect("deserialize snapshot");
        assert_eq!(state.resolved.len(), 2);
    }

    #[test]
    fn hil_snapshot_roundtrip() {
        let mut hil = HilComponent::new();

        hil.submit(ApprovalRequest::with_id(
            "req-1",
            "write",
            "Write file",
            serde_json::json!({}),
        ));
        hil.resolve("req-1", ApprovalResult::Approved)
            .expect("resolve req-1");

        hil.submit(ApprovalRequest::with_id(
            "req-2",
            "bash",
            "Run command",
            serde_json::json!({}),
        ));
        hil.resolve(
            "req-2",
            ApprovalResult::Rejected {
                reason: Some("nope".into()),
            },
        )
        .expect("resolve req-2");

        let snapshot = hil.snapshot().expect("snapshot for roundtrip");

        // Restore into a fresh HilComponent
        let mut hil2 = HilComponent::new();
        assert_eq!(hil2.resolved_count(), 0);

        hil2.restore(&snapshot).expect("restore snapshot");

        assert_eq!(hil2.resolved_count(), 2);
        let (_, result1) = hil2.get_resolved("req-1").expect("get restored req-1");
        assert!(result1.is_approved());
        let (_, result2) = hil2.get_resolved("req-2").expect("get restored req-2");
        assert!(result2.is_rejected());
    }

    #[test]
    fn hil_snapshot_pending_not_included() {
        let mut hil = HilComponent::new();

        // Submit but don't resolve
        hil.submit(ApprovalRequest::with_id(
            "pending-1",
            "write",
            "Write file",
            serde_json::json!({}),
        ));
        assert!(hil.has_pending());

        let snapshot = hil.snapshot().expect("snapshot with pending");
        let state: HilSnapshot = snapshot.to_state().expect("deserialize snapshot");

        // Pending should NOT be in snapshot
        assert!(state.resolved.is_empty());
    }

    #[test]
    fn hil_snapshot_fqn_mismatch() {
        let hil = HilComponent::new();
        let mut snapshot = hil.snapshot().expect("snapshot for mismatch test");
        snapshot.component_fqn = "wrong::component".to_string();

        let mut hil2 = HilComponent::new();
        let err = hil2.restore(&snapshot);
        assert!(err.is_err());
    }
}

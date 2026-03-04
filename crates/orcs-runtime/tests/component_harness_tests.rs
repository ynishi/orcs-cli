//! Integration tests using ComponentTestHarness.
//!
//! Demonstrates testing builtin components with the test harness.

use orcs_component::testing::ComponentTestHarness;
use orcs_component::{EventCategory, Status};
use orcs_event::SignalResponse;
use orcs_runtime::components::{HilComponent, NoopComponent};
use serde_json::{json, Value};

// =============================================================================
// NoopComponent Tests
// =============================================================================

mod noop_component {
    use super::*;

    #[tokio::test]
    async fn basic_echo() {
        let comp = NoopComponent::new("noop-test");
        let mut harness = ComponentTestHarness::new(comp);

        let result = harness
            .request(EventCategory::Echo, "any_operation", json!({"data": 123}))
            .await;

        assert!(result.is_ok());
        assert_eq!(
            result.expect("echo request should return Ok"),
            json!({"data": 123})
        );
    }

    #[test]
    fn veto_soft_cancel() {
        let comp = NoopComponent::new("noop-test");
        let mut harness = ComponentTestHarness::new(comp);

        assert_eq!(harness.status(), Status::Idle);

        let response = harness.veto();

        assert_eq!(response, SignalResponse::Handled);
        assert_eq!(harness.status(), Status::Idle);
    }

    #[test]
    fn cancel_handled() {
        let comp = NoopComponent::new("noop-test");
        let mut harness = ComponentTestHarness::new(comp);

        let response = harness.cancel();

        assert_eq!(response, SignalResponse::Handled);
        assert_eq!(harness.status(), Status::Idle);
    }

    #[tokio::test]
    async fn request_log_captured() {
        let comp = NoopComponent::new("noop-test");
        let mut harness = ComponentTestHarness::new(comp);

        harness
            .request(EventCategory::Echo, "op1", json!(1))
            .await
            .ok();
        harness
            .request(EventCategory::Echo, "op2", json!(2))
            .await
            .ok();
        harness
            .request(EventCategory::Echo, "op3", json!(3))
            .await
            .ok();

        assert_eq!(harness.request_log().len(), 3);
        assert_eq!(harness.request_log()[0].operation, "op1");
        assert_eq!(harness.request_log()[1].operation, "op2");
        assert_eq!(harness.request_log()[2].operation, "op3");
    }

    #[test]
    fn signal_log_captured() {
        let comp = NoopComponent::new("noop-test");
        let mut harness = ComponentTestHarness::new(comp);

        harness.cancel();
        harness.approve("test-approval");

        assert_eq!(harness.signal_log().len(), 2);
    }
}

// =============================================================================
// HilComponent Tests
// =============================================================================

mod hil_component {
    use super::*;

    fn create_approval_request(id: &str, operation: &str) -> Value {
        json!({
            "id": id,
            "operation": operation,
            "description": format!("{} operation", operation),
            "context": {},
            "created_at_ms": 0
        })
    }

    #[tokio::test]
    async fn submit_and_list() {
        let hil = HilComponent::new();
        let mut harness = ComponentTestHarness::new(hil);

        // Submit an approval request
        let result = harness
            .request(
                EventCategory::Hil,
                "submit",
                create_approval_request("req-1", "write"),
            )
            .await;

        assert!(result.is_ok());
        let response = result.expect("submit request should return Ok");
        assert_eq!(response["status"], "pending");

        // List pending requests
        let result = harness.request(EventCategory::Hil, "list", json!({})).await;
        assert!(result.is_ok());

        let list_response = result.expect("list request should return Ok");
        let pending = list_response["pending"]
            .as_array()
            .expect("pending field should be an array");
        assert_eq!(pending.len(), 1);
    }

    #[tokio::test]
    async fn submit_and_approve_via_signal() {
        let hil = HilComponent::new();
        let mut harness = ComponentTestHarness::new(hil);

        // Submit
        harness
            .request(
                EventCategory::Hil,
                "submit",
                create_approval_request("req-approve", "write"),
            )
            .await
            .expect("submit approval request should succeed");

        // Check status (pending)
        let result = harness
            .request(
                EventCategory::Hil,
                "status",
                json!({"approval_id": "req-approve"}),
            )
            .await;
        assert_eq!(
            result.expect("status request should return Ok")["status"],
            "pending"
        );

        // Approve via signal
        let response = harness.approve("req-approve");
        assert_eq!(response, SignalResponse::Handled);

        // Check status (resolved)
        let result = harness
            .request(
                EventCategory::Hil,
                "status",
                json!({"approval_id": "req-approve"}),
            )
            .await;
        assert_eq!(
            result.expect("status request after approve should return Ok")["status"],
            "resolved"
        );
    }

    #[tokio::test]
    async fn submit_and_reject_via_signal() {
        let hil = HilComponent::new();
        let mut harness = ComponentTestHarness::new(hil);

        // Submit
        harness
            .request(
                EventCategory::Hil,
                "submit",
                create_approval_request("req-reject", "bash"),
            )
            .await
            .expect("submit rejection request should succeed");

        // Reject via signal
        let response = harness.reject("req-reject", Some("Too dangerous".to_string()));
        assert_eq!(response, SignalResponse::Handled);

        // Check status (resolved)
        let result = harness
            .request(
                EventCategory::Hil,
                "status",
                json!({"approval_id": "req-reject"}),
            )
            .await;
        let status_response = result.expect("status request after reject should return Ok");
        assert_eq!(status_response["status"], "resolved");
        assert!(status_response["result"]["Rejected"].is_object());
    }

    #[test]
    fn approve_nonexistent_ignored() {
        let hil = HilComponent::new();
        let mut harness = ComponentTestHarness::new(hil);

        let response = harness.approve("nonexistent");
        assert_eq!(response, SignalResponse::Ignored);
    }

    #[tokio::test]
    async fn veto_rejects_all_pending() {
        let hil = HilComponent::new();
        let mut harness = ComponentTestHarness::new(hil);

        // Submit multiple requests
        harness
            .request(
                EventCategory::Hil,
                "submit",
                create_approval_request("req-1", "op1"),
            )
            .await
            .expect("submit first approval request should succeed");
        harness
            .request(
                EventCategory::Hil,
                "submit",
                create_approval_request("req-2", "op2"),
            )
            .await
            .expect("submit second approval request should succeed");

        // Verify pending
        let result = harness
            .request(EventCategory::Hil, "list", json!({}))
            .await
            .expect("list request should return Ok");
        assert_eq!(
            result["pending"]
                .as_array()
                .expect("pending field should be an array")
                .len(),
            2
        );

        // Veto (soft cancel: clears pending, returns to Idle)
        let response = harness.veto();
        assert_eq!(response, SignalResponse::Handled);
        assert_eq!(harness.status(), Status::Idle);

        // All pending should be rejected
        let result = harness
            .request(EventCategory::Hil, "list", json!({}))
            .await
            .expect("list request after veto should return Ok");
        assert_eq!(
            result["pending"]
                .as_array()
                .expect("pending field should be an array after veto")
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn unsupported_operation() {
        let hil = HilComponent::new();
        let mut harness = ComponentTestHarness::new(hil);

        let result = harness
            .request(EventCategory::Hil, "unknown_operation", json!({}))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn request_and_signal_log() {
        let hil = HilComponent::new();
        let mut harness = ComponentTestHarness::new(hil);

        // Perform operations
        harness
            .request(
                EventCategory::Hil,
                "submit",
                create_approval_request("req-log", "write"),
            )
            .await
            .expect("submit request for log test should succeed");
        harness.approve("req-log");
        harness
            .request(EventCategory::Hil, "list", json!({}))
            .await
            .expect("list request for log test should return Ok");

        // Check logs
        assert_eq!(harness.request_log().len(), 2);
        assert_eq!(harness.signal_log().len(), 1);

        // Clear and verify
        harness.clear_logs();
        assert_eq!(harness.request_log().len(), 0);
        assert_eq!(harness.signal_log().len(), 0);
    }
}

// =============================================================================
// Lifecycle Tests
// =============================================================================

mod lifecycle {
    use super::*;

    #[test]
    fn init_and_shutdown() {
        let comp = NoopComponent::new("lifecycle-test");
        let mut harness = ComponentTestHarness::new(comp);

        assert!(harness.init().is_ok());
        assert_eq!(harness.status(), Status::Idle);

        harness.shutdown();
        // NoopComponent doesn't change status on shutdown
        assert_eq!(harness.status(), Status::Idle);
    }

    #[test]
    fn abort_changes_status() {
        let comp = NoopComponent::new("abort-test");
        let mut harness = ComponentTestHarness::new(comp);

        assert_eq!(harness.status(), Status::Idle);

        harness.abort();

        assert_eq!(harness.status(), Status::Aborted);
    }

    #[test]
    fn id_accessible() {
        let comp = NoopComponent::new("id-test");
        let harness = ComponentTestHarness::new(comp);

        assert_eq!(harness.id().name, "id-test");
        assert_eq!(harness.id().namespace, "builtin");
    }
}

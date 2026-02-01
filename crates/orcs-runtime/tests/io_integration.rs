//! Integration tests for IO abstraction layer.
//!
//! Tests the complete flow: View → Bridge → Model

use orcs_event::SignalKind;
use orcs_runtime::components::{ApprovalRequest, HumanChannel};
use orcs_runtime::io::{ConsoleRenderer, IOInput, IOOutput, IOPort};
use orcs_types::{ChannelId, Principal, PrincipalId};

fn test_principal() -> Principal {
    Principal::User(PrincipalId::new())
}

/// Test basic input → signal conversion flow
#[tokio::test]
async fn view_to_bridge_input_flow() {
    let channel_id = ChannelId::new();
    let (port, input_handle, _output_handle) = IOPort::with_defaults(channel_id);
    let mut human_channel = HumanChannel::new(port, test_principal());

    // Set default approval ID for HIL responses
    human_channel.set_default_approval_id(Some("pending-approval-1".to_string()));

    // View layer sends input
    input_handle.send(IOInput::line("y")).await.unwrap();
    input_handle.send(IOInput::line("veto")).await.unwrap();

    // Small delay to ensure messages are buffered
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // Bridge layer processes input
    let (signals, commands) = human_channel.drain_input_to_signals();

    assert_eq!(signals.len(), 2);
    assert!(signals[0].is_approve());
    assert!(signals[1].is_veto());
    assert!(commands.is_empty());
}

/// Test output flow from Bridge to View
#[tokio::test]
async fn bridge_to_view_output_flow() {
    let channel_id = ChannelId::new();
    let (port, _input_handle, mut output_handle) = IOPort::with_defaults(channel_id);
    let human_channel = HumanChannel::new(port, test_principal());

    // Bridge layer sends output
    human_channel.info("Test message").await.unwrap();
    human_channel.warn("Warning!").await.unwrap();
    human_channel.error("Error occurred").await.unwrap();

    // View layer receives output
    let outputs = output_handle.drain();
    assert_eq!(outputs.len(), 3);

    // Verify output types
    for output in &outputs {
        assert!(matches!(output, IOOutput::Print { .. }));
    }
}

/// Test approval request flow
#[tokio::test]
async fn approval_request_flow() {
    let channel_id = ChannelId::new();
    let (port, input_handle, mut output_handle) = IOPort::with_defaults(channel_id);
    let mut human_channel = HumanChannel::new(port, test_principal());

    // Create and display an approval request
    let request = ApprovalRequest::with_id(
        "req-integration-test",
        "write",
        "Write to important file",
        serde_json::json!({"path": "/etc/config"}),
    );

    human_channel.show_approval_request(&request).await.unwrap();
    human_channel.set_default_approval_id(Some("req-integration-test".to_string()));

    // View receives the approval request display
    let output = output_handle.recv().await.unwrap();
    if let IOOutput::ShowApprovalRequest {
        id,
        operation,
        description,
    } = output
    {
        assert_eq!(id, "req-integration-test");
        assert_eq!(operation, "write");
        assert!(description.contains("important file"));
    } else {
        panic!("Expected ShowApprovalRequest");
    }

    // User approves
    input_handle.send(IOInput::line("y")).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    let (signals, _) = human_channel.drain_input_to_signals();
    assert_eq!(signals.len(), 1);
    assert!(signals[0].is_approve());

    if let SignalKind::Approve { approval_id } = &signals[0].kind {
        assert_eq!(approval_id, "req-integration-test");
    } else {
        panic!("Expected Approve signal");
    }

    // Confirm approval display
    human_channel
        .show_approved("req-integration-test")
        .await
        .unwrap();
    let output = output_handle.recv().await.unwrap();
    assert!(matches!(output, IOOutput::ShowApproved { .. }));
}

/// Test rejection flow with reason
#[tokio::test]
async fn rejection_flow_with_reason() {
    let channel_id = ChannelId::new();
    let (port, input_handle, mut output_handle) = IOPort::with_defaults(channel_id);
    let mut human_channel = HumanChannel::new(port, test_principal());

    // Set default approval ID
    human_channel.set_default_approval_id(Some("req-reject-test".to_string()));

    // User rejects with ID and reason
    input_handle
        .send(IOInput::line("n req-reject-test too-dangerous"))
        .await
        .unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    let (signals, _) = human_channel.drain_input_to_signals();
    assert_eq!(signals.len(), 1);
    assert!(signals[0].is_reject());

    if let SignalKind::Reject {
        approval_id,
        reason,
    } = &signals[0].kind
    {
        assert_eq!(approval_id, "req-reject-test");
        assert_eq!(reason, &Some("too-dangerous".to_string()));
    } else {
        panic!("Expected Reject signal");
    }

    // Confirm rejection display
    human_channel
        .show_rejected("req-reject-test", Some("too-dangerous"))
        .await
        .unwrap();
    let output = output_handle.recv().await.unwrap();
    if let IOOutput::ShowRejected { approval_id, reason } = output {
        assert_eq!(approval_id, "req-reject-test");
        assert_eq!(reason, Some("too-dangerous".to_string()));
    } else {
        panic!("Expected ShowRejected");
    }
}

/// Test pre-parsed signal from View layer
#[tokio::test]
async fn preparsed_signal_from_view() {
    let channel_id = ChannelId::new();
    let (port, input_handle, _output_handle) = IOPort::with_defaults(channel_id);
    let mut human_channel = HumanChannel::new(port, test_principal());

    // View layer sends a pre-parsed signal (e.g., from Ctrl+C handler)
    input_handle
        .send(IOInput::Signal(SignalKind::Veto))
        .await
        .unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    let (signals, _) = human_channel.drain_input_to_signals();
    assert_eq!(signals.len(), 1);
    assert!(signals[0].is_veto());
    assert!(signals[0].is_global());
}

/// Test EOF handling
#[tokio::test]
async fn eof_handling() {
    let channel_id = ChannelId::new();
    let (port, input_handle, _output_handle) = IOPort::with_defaults(channel_id);
    let mut human_channel = HumanChannel::new(port, test_principal());

    // View layer sends EOF
    input_handle.send(IOInput::Eof).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    let (signals, commands) = human_channel.drain_input_to_signals();
    assert!(signals.is_empty());
    assert_eq!(commands.len(), 1);
    assert!(matches!(
        commands[0],
        orcs_runtime::io::InputCommand::Quit
    ));
}

/// Test async recv_input
#[tokio::test]
async fn async_recv_input() {
    let channel_id = ChannelId::new();
    let (port, input_handle, _output_handle) = IOPort::with_defaults(channel_id);
    let mut human_channel = HumanChannel::new(port, test_principal());
    human_channel.set_default_approval_id(Some("async-test".to_string()));

    // Spawn a task to send input after a delay
    let handle = tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        input_handle.send(IOInput::line("y")).await.unwrap();
    });

    // Wait for input
    let result = human_channel.recv_input().await;
    assert!(result.is_some());
    let signal = result.unwrap().unwrap();
    assert!(signal.is_approve());

    handle.await.unwrap();
}

/// Test ConsoleRenderer integration
#[tokio::test]
async fn console_renderer_integration() {
    let channel_id = ChannelId::new();
    let (port, _input_handle, mut output_handle) = IOPort::with_defaults(channel_id);
    let human_channel = HumanChannel::new(port, test_principal());

    // Send various outputs
    human_channel.info("Info message").await.unwrap();
    human_channel.warn("Warning message").await.unwrap();
    human_channel.prompt("Enter value:").await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // Create renderer and drain outputs
    let renderer = ConsoleRenderer::new();
    let count = renderer.drain_and_render(&mut output_handle);
    assert_eq!(count, 3);
}

/// Test channel closed detection
#[tokio::test]
async fn channel_closed_detection() {
    let channel_id = ChannelId::new();
    let (port, input_handle, output_handle) = IOPort::with_defaults(channel_id);
    let human_channel = HumanChannel::new(port, test_principal());

    // Output channel is open
    assert!(!human_channel.is_output_closed());

    // Drop output handle
    drop(output_handle);

    // Output channel is now closed
    assert!(human_channel.is_output_closed());

    // Input handle should still work until port is dropped
    assert!(!input_handle.is_closed());
}

/// Test multiple input sources (cloned handles)
#[tokio::test]
async fn multiple_input_sources() {
    let channel_id = ChannelId::new();
    let (port, input_handle, _output_handle) = IOPort::with_defaults(channel_id);
    let mut human_channel = HumanChannel::new(port, test_principal());
    human_channel.set_default_approval_id(Some("multi-source".to_string()));

    // Clone the input handle (simulating multiple input sources)
    let input_handle2 = input_handle.clone();

    // Both handles send input
    input_handle.send(IOInput::line("y")).await.unwrap();
    input_handle2.send(IOInput::line("veto")).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    let (signals, _) = human_channel.drain_input_to_signals();
    assert_eq!(signals.len(), 2);
}

//! ClientRunner - ChannelRunner with IO bridging for Human interaction.
//!
//! Extends the basic runner with IO input/output capabilities.
//! The IO input loop is integrated into the main `select!` loop.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────┐
//! │                     ClientRunner                          │
//! │                                                           │
//! │  EventBus ───inject()────► event_rx                       │
//! │                                                           │
//! │  Human ────signal()──────► signal_rx ◄── broadcast        │
//! │                                                           │
//! │  View ─────input()───────► io_bridge.recv_input() ────┐   │
//! │                                                        │  │
//! │                              │                         │  │
//! │                              ▼                         ▼  │
//! │                        tokio::select! ◄────────────────┘  │
//! │                              │                             │
//! │                              ├─► Component.on_signal()     │
//! │                              ├─► Component.on_request()    │
//! │                              └─► IOBridge.show_*()         │
//! │                                                           │
//! │                              │                             │
//! │                              ▼                             │
//! │                    world_tx ───► WorldManager              │
//! └──────────────────────────────────────────────────────────┘
//! ```

use super::base::{ChannelHandle, Event, InboundEvent, RunnerResult};
use super::common::{
    determine_channel_action, is_channel_active, is_channel_paused, send_abort, send_transition,
    SignalAction,
};
use super::paused_queue::PausedEventQueue;
use crate::channel::command::{StateTransition, WorldCommand};
use crate::channel::World;
use crate::components::IOBridge;
use crate::engine::SharedChannelHandles;
use crate::io::{IOPort, InputCommand};
use orcs_event::{EventCategory, Signal, SignalKind};
use orcs_types::{ChannelId, ComponentId, Principal};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};
use tracing::{debug, info, warn};

/// Default event buffer size per channel.
const EVENT_BUFFER_SIZE: usize = 64;

/// FQN used by ClientRunner in RunnerResult (not a real component).
const CLIENT_RUNNER_FQN: &str = "io_bridge";

// --- Response handling ---

/// JSON field names for component responses.
#[allow(dead_code)]
mod response_fields {
    pub const STATUS: &str = "status";
    pub const PENDING_APPROVAL: &str = "pending_approval";
    pub const APPROVAL_ID: &str = "approval_id";
    pub const MESSAGE: &str = "message";
    pub const RESPONSE: &str = "response";
    pub const DATA: &str = "data";
}

/// Categorized component response for display routing.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentResponse<'a> {
    /// Awaiting human approval with ID and description.
    PendingApproval {
        approval_id: &'a str,
        description: &'a str,
    },
    /// Direct text response to display.
    TextResponse(&'a str),
    /// Error response from component.
    ///
    /// Triggered when response contains `{ "success": false, "error": "..." }`.
    ErrorResponse(&'a str),
    /// No displayable content.
    Empty,
}

#[allow(dead_code)]
impl<'a> ComponentResponse<'a> {
    /// Parses a JSON response into a categorized response.
    ///
    /// Supports multiple formats:
    /// - `{ "status": "pending_approval", "approval_id": "...", "message": "..." }`
    /// - `{ "success": false, "error": "..." }` → ErrorResponse
    /// - `{ "response": "..." }`
    /// - `{ "data": { "response": "..." } }`
    #[must_use]
    pub fn from_json(value: &'a serde_json::Value) -> Self {
        use response_fields::*;

        // Check for pending_approval status
        if value.get(STATUS).and_then(|v| v.as_str()) == Some(PENDING_APPROVAL) {
            if let Some(approval_id) = value.get(APPROVAL_ID).and_then(|v| v.as_str()) {
                let description = value
                    .get(MESSAGE)
                    .and_then(|v| v.as_str())
                    .unwrap_or("Awaiting approval");
                return Self::PendingApproval {
                    approval_id,
                    description,
                };
            }
        }

        // Check for error response: { "success": false, "error": "..." }
        if value.get("success").and_then(|v| v.as_bool()) == Some(false) {
            if let Some(error_msg) = value.get("error").and_then(|v| v.as_str()) {
                return Self::ErrorResponse(error_msg);
            }
            return Self::ErrorResponse("Unknown error");
        }

        // Check for direct response field
        if let Some(text) = value.get(RESPONSE).and_then(|v| v.as_str()) {
            return Self::TextResponse(text);
        }

        // Check for nested data.response
        if let Some(text) = value
            .get(DATA)
            .and_then(|d| d.get(RESPONSE))
            .and_then(|v| v.as_str())
        {
            return Self::TextResponse(text);
        }

        Self::Empty
    }
}
/// Configuration for creating a ClientRunner.
///
/// Groups World and Signal channel parameters to reduce argument count.
pub struct ClientRunnerConfig {
    /// Command sender for World modifications.
    pub world_tx: mpsc::Sender<WorldCommand>,
    /// Read-only World access.
    pub world: Arc<RwLock<World>>,
    /// Broadcast receiver for signals.
    pub signal_rx: broadcast::Receiver<Signal>,
    /// Shared channel handles for UserInput broadcast.
    pub channel_handles: SharedChannelHandles,
}

/// Runner with IO bridging for Human-interactive channels.
///
/// This runner is dedicated to bridging Human input/output with the system.
/// It does NOT hold a Component - instead it broadcasts UserInput events
/// to all channels and displays Output events from other channels.
///
/// # Architecture
///
/// ```text
/// Human ↔ ClientRunner (IOBridge only)
///            ↓ UserInput Event (broadcast)
///         EventBus
///            ↓
///         ChannelRunner(claude_cli, etc.)
///            ↓ Output Event
///         ClientRunner
///            ↓
///         Human (display)
/// ```
pub struct ClientRunner {
    /// This channel's ID.
    id: ChannelId,
    /// Sender for events (used for creating handle and output routing).
    event_tx: mpsc::Sender<InboundEvent>,
    /// Receiver for incoming events (Output from other channels).
    event_rx: mpsc::Receiver<InboundEvent>,
    /// Receiver for signals (broadcast).
    signal_rx: broadcast::Receiver<Signal>,
    /// Sender for World commands.
    world_tx: mpsc::Sender<WorldCommand>,
    /// Read-only World access.
    world: Arc<RwLock<World>>,
    /// Shared channel handles for UserInput broadcast.
    channel_handles: SharedChannelHandles,
    /// Queue for events received while paused.
    paused_queue: PausedEventQueue,

    // === IO-specific fields ===
    /// IO bridge for View-Model communication.
    io_bridge: IOBridge,
    /// Principal for signal creation from IO input.
    principal: Principal,
    /// Source ID for events (identifies this runner as event source).
    source_id: ComponentId,
}

impl ClientRunner {
    /// Creates a new ClientRunner with IO bridging (no component).
    ///
    /// This runner is dedicated to Human I/O. It broadcasts UserInput events
    /// to all channels and displays Output events received from other channels.
    ///
    /// # Arguments
    ///
    /// * `id` - The channel's ID
    /// * `config` - World and signal channel configuration
    /// * `io_port` - IO port for View communication
    /// * `principal` - Principal for signal creation
    #[must_use]
    pub fn new(
        id: ChannelId,
        config: ClientRunnerConfig,
        io_port: IOPort,
        principal: Principal,
    ) -> (Self, ChannelHandle) {
        let (event_tx, event_rx) = mpsc::channel(EVENT_BUFFER_SIZE);

        let runner = Self {
            id,
            event_tx: event_tx.clone(),
            event_rx,
            signal_rx: config.signal_rx,
            world_tx: config.world_tx,
            world: config.world,
            channel_handles: config.channel_handles,
            paused_queue: PausedEventQueue::new(),
            io_bridge: IOBridge::new(io_port),
            principal,
            source_id: ComponentId::builtin("io_bridge"),
        };

        let handle = ChannelHandle::new(id, event_tx);
        (runner, handle)
    }

    /// Returns this channel's ID.
    #[must_use]
    pub fn id(&self) -> ChannelId {
        self.id
    }

    /// Returns the event sender for this channel.
    ///
    /// This can be used to inject Output events into this runner for display.
    #[must_use]
    pub(crate) fn event_tx(&self) -> &mpsc::Sender<InboundEvent> {
        &self.event_tx
    }

    /// Returns a reference to the IO bridge.
    #[must_use]
    pub fn io_bridge(&self) -> &IOBridge {
        &self.io_bridge
    }

    /// Returns a mutable reference to the IO bridge.
    #[must_use]
    pub fn io_bridge_mut(&mut self) -> &mut IOBridge {
        &mut self.io_bridge
    }

    /// Runs the channel's event loop with IO integration.
    ///
    /// Handles three input sources concurrently:
    /// 1. Signals (highest priority - control messages)
    /// 2. Events (component work)
    /// 3. IO input (user interaction → converted to Signals)
    pub async fn run(mut self) -> RunnerResult {
        info!("ClientRunner {} started", self.id);

        loop {
            tokio::select! {
                // Priority 1: Signals (control)
                biased;

                signal = self.signal_rx.recv() => {
                    match signal {
                        Ok(sig) => {
                            if !self.handle_signal(sig).await {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            info!("ClientRunner {}: signal channel closed", self.id);
                            break;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("ClientRunner {}: lagged {} signals", self.id, n);
                        }
                    }
                }

                // Priority 2: Events (component work)
                event = self.event_rx.recv() => {
                    match event {
                        Some(evt) => {
                            if !self.handle_event(evt).await {
                                break;
                            }
                        }
                        None => {
                            info!("ClientRunner {}: event channel closed", self.id);
                            break;
                        }
                    }
                }

                // Priority 3: IO input (user interaction)
                io_result = self.io_bridge.recv_input(&self.principal) => {
                    match io_result {
                        Some(Ok(signal)) => {
                            // User input converted to Signal → process it
                            debug!("ClientRunner {}: IO input → {:?}", self.id, signal.kind);
                            if !self.handle_signal(signal).await {
                                break;
                            }
                        }
                        Some(Err(cmd)) => {
                            // Non-signal command (Quit, Unknown, etc.)
                            if !self.handle_io_command(cmd).await {
                                break;
                            }
                        }
                        None => {
                            info!("ClientRunner {}: IO closed", self.id);
                            break;
                        }
                    }
                }
            }

            // Check if channel is still active
            if !is_channel_active(&self.world, self.id).await {
                debug!("ClientRunner {}: channel no longer active", self.id);
                break;
            }
        }

        info!("ClientRunner {} stopped", self.id);

        RunnerResult {
            channel_id: self.id,
            component_fqn: CLIENT_RUNNER_FQN.to_string(),
            snapshot: None,
        }
    }

    /// Handles IO commands that don't map to Signals.
    ///
    /// # Command Routing
    ///
    /// | Command | Action |
    /// |---------|--------|
    /// | Quit | Abort channel |
    /// | Unknown | Broadcast as UserInput event to all channels |
    /// | Empty | Ignore |
    async fn handle_io_command(&mut self, cmd: InputCommand) -> bool {
        match cmd {
            InputCommand::Quit => {
                info!("ClientRunner {}: quit requested", self.id);
                send_abort(&self.world_tx, self.id, "user quit").await;
                false
            }
            InputCommand::Unknown { input } => {
                // Broadcast user message as UserInput event
                self.handle_user_message(&input);
                true
            }
            InputCommand::Empty => {
                // Blank line - ignore
                true
            }
            _ => true,
        }
    }

    /// Handles user message input by broadcasting to all channels.
    ///
    /// Creates a UserInput Event and broadcasts to all registered channels
    /// via the shared channel handles. Components that subscribe to
    /// `UserInput` category will receive this event.
    fn handle_user_message(&self, message: &str) {
        debug!("ClientRunner {}: user message: {}", self.id, message);

        // Create UserInput event
        let event = Event {
            category: EventCategory::UserInput,
            operation: "input".to_string(),
            source: self.source_id.clone(),
            payload: serde_json::json!({
                "message": message
            }),
        };

        // Broadcast to all channels
        let handles = self.channel_handles.read().expect("lock poisoned");
        let mut delivered = 0;
        for (channel_id, handle) in handles.iter() {
            // Skip self to avoid echo
            if *channel_id == self.id {
                continue;
            }
            if handle.try_inject(event.clone()).is_ok() {
                delivered += 1;
            }
        }
        debug!(
            "ClientRunner {}: broadcast UserInput to {} channels",
            self.id, delivered
        );
    }

    /// Handles an incoming signal.
    ///
    /// Includes IO feedback for approval-related signals.
    /// Note: This runner has no component, so signal handling is simplified.
    async fn handle_signal(&mut self, signal: Signal) -> bool {
        debug!(
            "ClientRunner {}: received signal {:?}",
            self.id, signal.kind
        );

        // Check if signal affects this channel
        if !signal.affects_channel(self.id) {
            return true;
        }

        // Determine channel-level action with IO feedback
        let action = determine_channel_action(&signal.kind);
        match action {
            SignalAction::Stop { reason } => {
                // Show rejection feedback if applicable
                if let SignalKind::Reject {
                    approval_id,
                    reason: rej_reason,
                } = &signal.kind
                {
                    let _ = self
                        .io_bridge
                        .show_rejected(approval_id, rej_reason.as_deref())
                        .await;
                }
                info!("ClientRunner {}: stopping - {}", self.id, reason);
                send_abort(&self.world_tx, self.id, &reason).await;
                return false;
            }
            SignalAction::Transition(transition) => {
                // Show approval feedback
                if let SignalKind::Approve { approval_id } = &signal.kind {
                    let _ = self.io_bridge.show_approved(approval_id).await;
                }

                send_transition(&self.world_tx, self.id, transition.clone()).await;

                // Drain paused queue on resume
                if matches!(transition, StateTransition::Resume) {
                    self.drain_paused_queue().await;
                }
            }
            SignalAction::Continue => {}
        }

        true
    }

    /// Handles an incoming event.
    ///
    /// ClientRunner has no subscription filter — it processes all events
    /// regardless of the Broadcast/Direct distinction.
    async fn handle_event(&mut self, inbound: InboundEvent) -> bool {
        let event = inbound.into_event();

        debug!(
            "ClientRunner {}: received event {:?} op={}",
            self.id, event.category, event.operation
        );

        // Queue events while paused
        if is_channel_paused(&self.world, self.id).await {
            self.paused_queue
                .try_enqueue(event, "ClientRunner", self.id);
            return true;
        }

        self.process_event(event).await;
        true
    }

    /// Processes a single event.
    ///
    /// ClientRunner only handles Output events (for display).
    /// Other events are ignored as this runner has no component.
    async fn process_event(&self, event: Event) {
        // Handle Output category: send to IOBridge for display
        if event.category == EventCategory::Output {
            self.handle_output_event(&event).await;
            return;
        }

        // Ignore other events - this runner has no component
        debug!(
            "ClientRunner {}: ignoring non-Output event {:?}",
            self.id, event.category
        );
    }

    /// Handles Output category events by sending to IOBridge.
    async fn handle_output_event(&self, event: &Event) {
        // Check for approval_request type events
        if event.payload.get("type").and_then(|v| v.as_str()) == Some("approval_request") {
            let approval_id = event.payload["approval_id"].as_str().unwrap_or("");
            let operation = event.payload["operation"].as_str().unwrap_or("exec");
            let description = event.payload["description"].as_str().unwrap_or("");

            let request = crate::components::ApprovalRequest::with_id(
                approval_id,
                operation,
                description,
                event.payload.clone(),
            );
            let _ = self.io_bridge.show_approval_request(&request).await;
            return;
        }

        let message = event
            .payload
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let level = event
            .payload
            .get("level")
            .and_then(|v| v.as_str())
            .unwrap_or("info");

        match level {
            "warn" | "warning" => {
                let _ = self.io_bridge.warn(message).await;
            }
            "error" => {
                let _ = self.io_bridge.error(message).await;
            }
            _ => {
                let _ = self.io_bridge.info(message).await;
            }
        }
    }

    /// Drains the paused queue and processes all queued events.
    async fn drain_paused_queue(&mut self) {
        // Collect events first to avoid borrow issues with async process_event
        let events: Vec<_> = self.paused_queue.drain("ClientRunner", self.id).collect();

        for event in events {
            self.process_event(event).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::config::ChannelConfig;
    use crate::channel::manager::WorldManager;
    use crate::io::IOPort;
    use orcs_types::PrincipalId;
    use std::collections::HashMap;
    use std::sync::RwLock as StdRwLock;

    fn test_principal() -> Principal {
        Principal::User(PrincipalId::new())
    }

    async fn setup() -> (
        tokio::task::JoinHandle<()>,
        mpsc::Sender<WorldCommand>,
        Arc<RwLock<World>>,
        broadcast::Sender<Signal>,
        ChannelId,
    ) {
        let mut world = World::new();
        let io = world.create_channel(ChannelConfig::interactive());

        let (manager, world_tx) = WorldManager::with_world(world);
        let world_handle = manager.world();

        let manager_task = tokio::spawn(manager.run());

        let (signal_tx, _) = broadcast::channel(64);

        (manager_task, world_tx, world_handle, signal_tx, io)
    }

    async fn teardown(
        manager_task: tokio::task::JoinHandle<()>,
        world_tx: mpsc::Sender<WorldCommand>,
    ) {
        let _ = world_tx.send(WorldCommand::Shutdown).await;
        let _ = manager_task.await;
    }

    fn make_config(
        world_tx: mpsc::Sender<WorldCommand>,
        world: Arc<RwLock<World>>,
        signal_tx: &broadcast::Sender<Signal>,
    ) -> ClientRunnerConfig {
        let channel_handles: SharedChannelHandles = Arc::new(StdRwLock::new(HashMap::new()));
        ClientRunnerConfig {
            world_tx,
            world,
            signal_rx: signal_tx.subscribe(),
            channel_handles,
        }
    }

    #[tokio::test]
    async fn client_runner_creation() {
        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let (port, _input, _output) = IOPort::with_defaults(primary);
        let config = make_config(world_tx.clone(), world, &signal_tx);
        let (runner, handle) = ClientRunner::new(primary, config, port, test_principal());

        assert_eq!(runner.id(), primary);
        assert_eq!(handle.id, primary);

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn client_runner_handles_io_input() {
        use crate::io::IOInput;

        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let (port, input_handle, _output_handle) = IOPort::with_defaults(primary);
        let config = make_config(world_tx.clone(), world, &signal_tx);
        let (runner, _handle) = ClientRunner::new(primary, config, port, test_principal());

        let runner_task = tokio::spawn(runner.run());
        tokio::task::yield_now().await;

        // Send quit command via IO
        input_handle.send(IOInput::line("q")).await.unwrap();

        // Runner should stop
        let result = tokio::time::timeout(std::time::Duration::from_millis(200), runner_task).await;
        assert!(result.is_ok());

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn client_runner_handles_approval() {
        use crate::io::{IOInput, InputContext};

        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let (port, input_handle, mut output_handle) = IOPort::with_defaults(primary);
        let config = make_config(world_tx.clone(), world, &signal_tx);
        let (runner, _handle) = ClientRunner::new(primary, config, port, test_principal());

        let runner_task = tokio::spawn(runner.run());
        tokio::task::yield_now().await;

        // Send approval via IO with context
        let ctx = InputContext::with_approval_id("req-123");
        input_handle
            .send(IOInput::line_with_context("y", ctx))
            .await
            .unwrap();

        // Wait for feedback
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Check for approval feedback (try_recv returns Option)
        if let Some(output) = output_handle.try_recv() {
            // Should have received approval feedback
            debug!("Received output: {:?}", output);
        }

        // Cleanup
        signal_tx
            .send(Signal::cancel(primary, Principal::System))
            .unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_millis(100), runner_task).await;

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn client_runner_handles_veto() {
        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let (port, _input, _output) = IOPort::with_defaults(primary);
        let config = make_config(world_tx.clone(), world, &signal_tx);
        let (runner, _handle) = ClientRunner::new(primary, config, port, test_principal());

        let runner_task = tokio::spawn(runner.run());
        tokio::task::yield_now().await;

        // Send veto
        signal_tx.send(Signal::veto(Principal::System)).unwrap();

        // Runner should stop
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), runner_task).await;
        assert!(result.is_ok());

        teardown(manager_task, world_tx).await;
    }

    // NOTE: ClientRunner no longer owns a Component.
    // UserInput is broadcast to all channels, and Output events are received
    // from other channels for display. Component-specific tests have been
    // moved to ChannelRunner tests.

    // --- ComponentResponse tests ---

    mod component_response_tests {
        use super::*;
        use serde_json::json;

        #[test]
        fn from_json_pending_approval() {
            let json = json!({
                "status": "pending_approval",
                "approval_id": "req-123",
                "message": "Confirm action?"
            });

            let response = ComponentResponse::from_json(&json);

            assert_eq!(
                response,
                ComponentResponse::PendingApproval {
                    approval_id: "req-123",
                    description: "Confirm action?"
                }
            );
        }

        #[test]
        fn from_json_pending_approval_default_message() {
            let json = json!({
                "status": "pending_approval",
                "approval_id": "req-456"
            });

            let response = ComponentResponse::from_json(&json);

            assert_eq!(
                response,
                ComponentResponse::PendingApproval {
                    approval_id: "req-456",
                    description: "Awaiting approval"
                }
            );
        }

        #[test]
        fn from_json_pending_approval_missing_id_returns_empty() {
            let json = json!({
                "status": "pending_approval"
                // Missing approval_id
            });

            let response = ComponentResponse::from_json(&json);

            assert_eq!(response, ComponentResponse::Empty);
        }

        #[test]
        fn from_json_direct_response() {
            let json = json!({
                "response": "Hello, world!"
            });

            let response = ComponentResponse::from_json(&json);

            assert_eq!(response, ComponentResponse::TextResponse("Hello, world!"));
        }

        #[test]
        fn from_json_nested_data_response() {
            let json = json!({
                "data": {
                    "response": "Nested response",
                    "source": "test"
                }
            });

            let response = ComponentResponse::from_json(&json);

            assert_eq!(response, ComponentResponse::TextResponse("Nested response"));
        }

        #[test]
        fn from_json_empty_object() {
            let json = json!({});

            let response = ComponentResponse::from_json(&json);

            assert_eq!(response, ComponentResponse::Empty);
        }

        #[test]
        fn from_json_unrelated_fields() {
            let json = json!({
                "status": "completed",
                "result": 42
            });

            let response = ComponentResponse::from_json(&json);

            assert_eq!(response, ComponentResponse::Empty);
        }

        #[test]
        fn from_json_priority_pending_over_response() {
            let json = json!({
                "status": "pending_approval",
                "approval_id": "req-789",
                "response": "This should be ignored"
            });

            let response = ComponentResponse::from_json(&json);

            assert_eq!(
                response,
                ComponentResponse::PendingApproval {
                    approval_id: "req-789",
                    description: "Awaiting approval"
                }
            );
        }

        #[test]
        fn from_json_response_priority_over_nested() {
            let json = json!({
                "response": "Direct",
                "data": {
                    "response": "Nested"
                }
            });

            let response = ComponentResponse::from_json(&json);

            assert_eq!(response, ComponentResponse::TextResponse("Direct"));
        }

        #[test]
        fn from_json_error_response() {
            let json = json!({
                "success": false,
                "error": "Command failed"
            });

            let response = ComponentResponse::from_json(&json);

            assert_eq!(response, ComponentResponse::ErrorResponse("Command failed"));
        }

        #[test]
        fn from_json_error_response_no_message() {
            let json = json!({
                "success": false
            });

            let response = ComponentResponse::from_json(&json);

            assert_eq!(response, ComponentResponse::ErrorResponse("Unknown error"));
        }

        #[test]
        fn from_json_success_true_not_error() {
            let json = json!({
                "success": true,
                "response": "Operation succeeded"
            });

            let response = ComponentResponse::from_json(&json);

            assert_eq!(
                response,
                ComponentResponse::TextResponse("Operation succeeded")
            );
        }

        #[test]
        fn from_json_pending_priority_over_error() {
            let json = json!({
                "status": "pending_approval",
                "approval_id": "req-999",
                "success": false,
                "error": "This should be ignored"
            });

            let response = ComponentResponse::from_json(&json);

            assert_eq!(
                response,
                ComponentResponse::PendingApproval {
                    approval_id: "req-999",
                    description: "Awaiting approval"
                }
            );
        }
    }
}

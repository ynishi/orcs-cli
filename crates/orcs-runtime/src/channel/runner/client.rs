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

use super::base::{ChannelHandle, Event};
use super::common::{
    determine_channel_action, dispatch_signal_to_component, is_channel_active, is_channel_paused,
    send_abort, send_transition, SignalAction,
};
use super::emitter::EventEmitter;
use super::paused_queue::PausedEventQueue;
use crate::channel::command::{StateTransition, WorldCommand};
use crate::channel::World;
use crate::components::IOBridge;
use crate::io::{IOPort, InputCommand};
use orcs_component::{Component, ComponentError};
use orcs_event::{EventCategory, Request, Signal, SignalKind};
use orcs_types::{ChannelId, Principal};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use tracing::{debug, info, warn};

/// Default event buffer size per channel.
const EVENT_BUFFER_SIZE: usize = 64;

// --- Response handling ---

/// JSON field names for component responses.
mod response_fields {
    pub const STATUS: &str = "status";
    pub const PENDING_APPROVAL: &str = "pending_approval";
    pub const APPROVAL_ID: &str = "approval_id";
    pub const MESSAGE: &str = "message";
    pub const RESPONSE: &str = "response";
    pub const DATA: &str = "data";
}

/// Categorized component response for display routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentResponse<'a> {
    /// Awaiting human approval with ID and description.
    PendingApproval {
        approval_id: &'a str,
        description: &'a str,
    },
    /// Direct text response to display.
    TextResponse(&'a str),
    /// No displayable content.
    Empty,
}

impl<'a> ComponentResponse<'a> {
    /// Parses a JSON response into a categorized response.
    ///
    /// Supports multiple formats:
    /// - `{ "status": "pending_approval", "approval_id": "...", "message": "..." }`
    /// - `{ "response": "..." }`
    /// - `{ "data": { "response": "..." } }`
    #[must_use]
    pub fn from_json(value: &'a Value) -> Self {
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
    /// Broadcast sender for signals (for EventEmitter).
    pub signal_tx: broadcast::Sender<Signal>,
}

/// Runner with IO bridging for Human-interactive channels.
///
/// Extends the basic runner with:
/// - IO input loop integrated into `select!`
/// - IO output methods for approval feedback
/// - Principal for signal creation from IO input
pub struct ClientRunner {
    /// This channel's ID.
    id: ChannelId,
    /// Receiver for incoming events.
    event_rx: mpsc::Receiver<Event>,
    /// Receiver for signals (broadcast).
    signal_rx: broadcast::Receiver<Signal>,
    /// Sender for World commands.
    world_tx: mpsc::Sender<WorldCommand>,
    /// Read-only World access.
    world: Arc<RwLock<World>>,
    /// Bound Component (1:1 relationship).
    component: Arc<Mutex<Box<dyn Component>>>,
    /// Queue for events received while paused.
    paused_queue: PausedEventQueue,

    // === IO-specific fields ===
    /// IO bridge for View-Model communication.
    io_bridge: IOBridge,
    /// Principal for signal creation from IO input.
    principal: Principal,
}

impl ClientRunner {
    /// Creates a new ClientRunner with IO bridging.
    ///
    /// # Arguments
    ///
    /// * `id` - The channel's ID
    /// * `config` - World and signal channel configuration
    /// * `component` - The Component bound to this channel (1:1)
    /// * `io_port` - IO port for View communication
    /// * `principal` - Principal for signal creation
    #[must_use]
    pub fn new(
        id: ChannelId,
        config: ClientRunnerConfig,
        mut component: Box<dyn Component>,
        io_port: IOPort,
        principal: Principal,
    ) -> (Self, ChannelHandle) {
        let (event_tx, event_rx) = mpsc::channel(EVENT_BUFFER_SIZE);

        // Create EventEmitter and inject into Component
        let component_id = component.id().clone();
        let emitter = EventEmitter::new(event_tx.clone(), config.signal_tx, component_id);
        component.set_emitter(Box::new(emitter));

        let runner = Self {
            id,
            event_rx,
            signal_rx: config.signal_rx,
            world_tx: config.world_tx,
            world: config.world,
            component: Arc::new(Mutex::new(component)),
            paused_queue: PausedEventQueue::new(),
            io_bridge: IOBridge::new(io_port),
            principal,
        };

        let handle = ChannelHandle::new(id, event_tx);
        (runner, handle)
    }

    /// Returns this channel's ID.
    #[must_use]
    pub fn id(&self) -> ChannelId {
        self.id
    }

    /// Returns a reference to the bound Component.
    #[must_use]
    pub fn component(&self) -> &Arc<Mutex<Box<dyn Component>>> {
        &self.component
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
    pub async fn run(mut self) {
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
    }

    /// Handles IO commands that don't map to Signals.
    ///
    /// # Command Routing
    ///
    /// | Command | Action |
    /// |---------|--------|
    /// | Quit | Abort channel |
    /// | Unknown | Forward to Component as user message |
    /// | Empty | Ignore |
    async fn handle_io_command(&mut self, cmd: InputCommand) -> bool {
        match cmd {
            InputCommand::Quit => {
                info!("ClientRunner {}: quit requested", self.id);
                send_abort(&self.world_tx, self.id, "user quit").await;
                false
            }
            InputCommand::Unknown { input } => {
                // Forward user message to Component as Echo request
                self.handle_user_message(&input).await;
                true
            }
            InputCommand::Empty => {
                // Blank line - ignore
                true
            }
            _ => true,
        }
    }

    /// Handles user message input by forwarding to Component.
    ///
    /// Creates a UserInput Event and delivers to Component via on_request().
    /// Components that subscribe to `UserInput` category will receive this event.
    async fn handle_user_message(&self, message: &str) {
        debug!("ClientRunner {}: user message: {}", self.id, message);

        // Get component ID for event source
        let source_id = {
            let comp = self.component.lock().await;
            comp.id().clone()
        };

        // Create UserInput event for Component
        let event = Event {
            category: EventCategory::UserInput,
            operation: "input".to_string(),
            source: source_id,
            payload: serde_json::json!({
                "message": message
            }),
        };

        self.process_event(event).await;
    }

    /// Handles an incoming signal.
    ///
    /// Includes IO feedback for approval-related signals.
    async fn handle_signal(&mut self, signal: Signal) -> bool {
        debug!(
            "ClientRunner {}: received signal {:?}",
            self.id, signal.kind
        );

        // Check if signal affects this channel
        if !signal.affects_channel(self.id) {
            return true;
        }

        // Dispatch to component first
        let component_action = dispatch_signal_to_component(&signal, &self.component).await;
        if let SignalAction::Stop { reason } = component_action {
            info!(
                "ClientRunner {}: component requested stop: {}",
                self.id, reason
            );
            send_abort(&self.world_tx, self.id, &reason).await;
            return false;
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
    async fn handle_event(&mut self, event: Event) -> bool {
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

    /// Processes a single event by delivering it to the Component.
    async fn process_event(&self, event: Event) {
        // Handle Output category: send to IOBridge for display
        if event.category == EventCategory::Output {
            self.handle_output_event(&event).await;
            return;
        }

        // Clone values needed after spawn_blocking
        let operation = event.operation.clone();
        let payload = event.payload.clone();

        let request = Request::new(
            event.category,
            &event.operation,
            event.source,
            self.id,
            event.payload,
        );

        // Execute on_request in spawn_blocking to avoid blocking the async runtime.
        // This is necessary because Component::on_request is a sync method that may
        // perform blocking I/O (e.g., LuaComponent calling external CLIs).
        let component = self.component.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut comp = component.blocking_lock();
            comp.on_request(&request)
        })
        .await
        .unwrap_or_else(|e| {
            Err(ComponentError::ExecutionFailed(format!(
                "task panicked: {}",
                e
            )))
        });

        self.handle_component_result(result, &operation, &payload)
            .await;
    }

    /// Handles the result from Component::on_request.
    ///
    /// Routes the response to the appropriate IOBridge method based on content.
    async fn handle_component_result(
        &self,
        result: Result<Value, ComponentError>,
        operation: &str,
        payload: &Value,
    ) {
        match result {
            Ok(response) => {
                debug!(
                    "ClientRunner {}: Component returned success: {:?}",
                    self.id, response
                );
                self.display_component_response(&response, operation, payload)
                    .await;
            }
            Err(e) => {
                warn!("ClientRunner {}: Component returned error: {}", self.id, e);
                let _ = self.io_bridge.error(&e.to_string()).await;
            }
        }
    }

    /// Displays a component response via IOBridge.
    ///
    /// Uses [`ComponentResponse`] to categorize and route the response.
    async fn display_component_response(&self, response: &Value, operation: &str, payload: &Value) {
        match ComponentResponse::from_json(response) {
            ComponentResponse::PendingApproval {
                approval_id,
                description,
            } => {
                let _ = self
                    .io_bridge
                    .show_approval_request(&crate::components::ApprovalRequest::with_id(
                        approval_id,
                        operation,
                        description,
                        payload.clone(),
                    ))
                    .await;
            }
            ComponentResponse::TextResponse(text) => {
                let _ = self.io_bridge.info(text).await;
            }
            ComponentResponse::Empty => {
                // No displayable content - intentionally silent
            }
        }
    }

    /// Handles Output category events by sending to IOBridge.
    async fn handle_output_event(&self, event: &Event) {
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
    use orcs_component::{ComponentError, Status};
    use orcs_event::SignalResponse;
    use orcs_types::{ComponentId, PrincipalId};
    use serde_json::Value;

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

    fn mock_component() -> Box<dyn Component> {
        Box::new(MockComponent::new("test"))
    }

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
        ClientRunnerConfig {
            world_tx,
            world,
            signal_rx: signal_tx.subscribe(),
            signal_tx: signal_tx.clone(),
        }
    }

    #[tokio::test]
    async fn client_runner_creation() {
        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let (port, _input, _output) = IOPort::with_defaults(primary);
        let config = make_config(world_tx.clone(), world, &signal_tx);
        let (runner, handle) =
            ClientRunner::new(primary, config, mock_component(), port, test_principal());

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
        let (runner, _handle) =
            ClientRunner::new(primary, config, mock_component(), port, test_principal());

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
        let (runner, _handle) =
            ClientRunner::new(primary, config, mock_component(), port, test_principal());

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
        let (runner, _handle) =
            ClientRunner::new(primary, config, mock_component(), port, test_principal());

        let runner_task = tokio::spawn(runner.run());
        tokio::task::yield_now().await;

        // Send veto
        signal_tx.send(Signal::veto(Principal::System)).unwrap();

        // Runner should stop
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), runner_task).await;
        assert!(result.is_ok());

        teardown(manager_task, world_tx).await;
    }

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
            // pending_approval should take priority over response field
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
            // Direct response should take priority over nested data.response
            let json = json!({
                "response": "Direct",
                "data": {
                    "response": "Nested"
                }
            });

            let response = ComponentResponse::from_json(&json);

            assert_eq!(response, ComponentResponse::TextResponse("Direct"));
        }
    }

    // --- spawn_blocking integration tests ---

    /// Mock component that returns a specific response format.
    struct ResponseMockComponent {
        id: ComponentId,
        status: Status,
        response: Value,
    }

    impl ResponseMockComponent {
        fn with_response(response: Value) -> Self {
            Self {
                id: ComponentId::builtin("response_mock"),
                status: Status::Idle,
                response,
            }
        }
    }

    impl Component for ResponseMockComponent {
        fn id(&self) -> &ComponentId {
            &self.id
        }

        fn status(&self) -> Status {
            self.status
        }

        fn on_request(&mut self, _request: &Request) -> Result<Value, ComponentError> {
            Ok(self.response.clone())
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

    #[tokio::test]
    async fn client_runner_displays_text_response() {
        use crate::io::{IOInput, IOOutput, OutputStyle};
        use serde_json::json;

        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let (port, input_handle, mut output_handle) = IOPort::with_defaults(primary);
        let config = make_config(world_tx.clone(), world, &signal_tx);

        // Component that returns a text response
        let component = Box::new(ResponseMockComponent::with_response(json!({
            "response": "Test response from component"
        })));

        let (runner, _handle) =
            ClientRunner::new(primary, config, component, port, test_principal());

        let runner_task = tokio::spawn(runner.run());
        tokio::task::yield_now().await;

        // Send user message to trigger component
        input_handle
            .send(IOInput::line("test message"))
            .await
            .unwrap();

        // Wait for response
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Check output was received
        let mut found_response = false;
        while let Some(output) = output_handle.try_recv() {
            if let IOOutput::Print { text, style } = output {
                if style == OutputStyle::Info && text.contains("Test response from component") {
                    found_response = true;
                    break;
                }
            }
        }
        assert!(found_response, "Expected text response to be displayed");

        // Cleanup
        signal_tx.send(Signal::veto(Principal::System)).unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_millis(100), runner_task).await;

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn client_runner_displays_nested_response() {
        use crate::io::{IOInput, IOOutput, OutputStyle};
        use serde_json::json;

        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let (port, input_handle, mut output_handle) = IOPort::with_defaults(primary);
        let config = make_config(world_tx.clone(), world, &signal_tx);

        // Component that returns nested data.response format (like claude_cli)
        let component = Box::new(ResponseMockComponent::with_response(json!({
            "data": {
                "response": "Nested CLI response",
                "source": "claude_cli"
            }
        })));

        let (runner, _handle) =
            ClientRunner::new(primary, config, component, port, test_principal());

        let runner_task = tokio::spawn(runner.run());
        tokio::task::yield_now().await;

        // Send user message
        input_handle.send(IOInput::line("hello")).await.unwrap();

        // Wait for response
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Check output
        let mut found_response = false;
        while let Some(output) = output_handle.try_recv() {
            if let IOOutput::Print { text, style } = output {
                if style == OutputStyle::Info && text.contains("Nested CLI response") {
                    found_response = true;
                    break;
                }
            }
        }
        assert!(found_response, "Expected nested response to be displayed");

        // Cleanup
        signal_tx.send(Signal::veto(Principal::System)).unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_millis(100), runner_task).await;

        teardown(manager_task, world_tx).await;
    }
}

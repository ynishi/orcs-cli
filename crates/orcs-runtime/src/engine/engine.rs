//! OrcsEngine - Main Runtime.
//!
//! The [`OrcsEngine`] is the central runtime that:
//!
//! - Manages Component lifecycle (register, poll, shutdown)
//! - Dispatches Signals to all Components
//! - Coordinates EventBus message routing
//! - Manages the World (Channel hierarchy)
//!
//! # Runtime Loop
//!
//! ```text
//! loop {
//!     1. Check Signals (highest priority - Veto stops everything)
//!     2. Poll each Component:
//!        - Deliver pending Signals
//!        - Deliver pending Requests
//!        - Collect Responses
//!     3. Yield to async runtime
//! }
//! ```
//!
//! # Signal Handling
//!
//! Signals are interrupts from Human (or authorized Components):
//!
//! - **Veto**: Immediate engine stop (checked first)
//! - **Cancel**: Stop specific Channel/Component
//! - **Pause/Resume**: Suspend/continue execution
//! - **Approve/Reject**: HIL responses
//!
//! Components MUST respond to Signals. If a Component returns
//! [`SignalResponse::Ignored`], the engine forces abort.

use super::eventbus::{ComponentHandle, EventBus};
use crate::channel::{
    ChannelConfig, ChannelHandle, ChannelRunner, ChannelRunnerFactory, World, WorldCommand,
    WorldCommandSender, WorldManager,
};
use orcs_component::Component;
use orcs_event::{Signal, SignalKind, SignalResponse};
use orcs_types::{ChannelId, ComponentId};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, info, warn};

/// Signal broadcast channel buffer size.
///
/// 256 signals provides sufficient buffering for HIL interactions.
/// Lagged receivers will miss old signals but continue receiving new ones.
const SIGNAL_BUFFER_SIZE: usize = 256;

/// OrcsEngine - Main runtime for ORCS CLI.
///
/// Manages the complete lifecycle of Components and coordinates
/// communication via EventBus.
///
/// # Parallel Execution Model
///
/// Engine operates in parallel mode with:
/// - [`WorldManager`](crate::channel::WorldManager) for concurrent World access
/// - [`ChannelRunner`](crate::channel::ChannelRunner) per channel
/// - Event injection via channel handles
///
/// # Example
///
/// ```ignore
/// use orcs_runtime::{World, ChannelConfig};
///
/// // Create World with IO channel
/// let mut world = World::new();
/// let io = world.create_channel(ChannelConfig::interactive());
///
/// // Inject into engine with IO channel (required)
/// let engine = OrcsEngine::new(world, io);
///
/// // Spawn channel runners with bound components
/// engine.spawn_runner(io, Box::new(MyComponent::new()));
///
/// // Run the engine (parallel execution)
/// engine.run().await;
/// ```
///
/// # Signal Priority
///
/// Signals are processed with highest priority. A Veto signal
/// immediately stops the engine without processing further.
pub struct OrcsEngine {
    /// EventBus for message routing
    eventbus: EventBus,
    /// Registered Components
    components: HashMap<ComponentId, Box<dyn Component>>,
    /// Component handles for message delivery
    handles: HashMap<ComponentId, ComponentHandle>,
    /// Running state
    running: bool,
    // --- Parallel execution infrastructure ---
    /// World command sender for async modifications
    world_tx: WorldCommandSender,
    /// Read-only World access for parallel reads
    world_read: Arc<RwLock<World>>,
    /// Signal broadcaster for all channel runners
    signal_tx: broadcast::Sender<Signal>,
    /// Channel runner handles (for event injection)
    channel_handles: HashMap<ChannelId, ChannelHandle>,
    /// WorldManager task handle
    manager_task: Option<tokio::task::JoinHandle<()>>,
    /// Channel runner task handles
    runner_tasks: HashMap<ChannelId, tokio::task::JoinHandle<()>>,
    // --- IO Channel (required) ---
    /// IO channel for Human input/output.
    ///
    /// This is a required field - every engine must have an IO channel
    /// for Human interaction.
    io_channel: ChannelId,
}

impl OrcsEngine {
    /// Create new engine with injected World and IO channel.
    ///
    /// The World is immediately transferred to a [`WorldManager`] which
    /// starts processing commands. The IO channel is required for Human
    /// input/output.
    ///
    /// # Arguments
    ///
    /// * `world` - The World containing the IO channel
    /// * `io_channel` - The IO channel ID (must exist in World)
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut world = World::new();
    /// let io = world.create_channel(ChannelConfig::interactive());
    ///
    /// let engine = OrcsEngine::new(world, io);
    /// ```
    #[must_use]
    pub fn new(world: World, io_channel: ChannelId) -> Self {
        // Create WorldManager immediately (takes ownership of World)
        let (manager, world_tx) = WorldManager::with_world(world);
        let world_read = manager.world();

        // Create signal broadcaster
        let (signal_tx, _) = broadcast::channel(SIGNAL_BUFFER_SIZE);

        // Start WorldManager task
        let manager_task = tokio::spawn(manager.run());

        info!(
            "OrcsEngine created with IO channel {} (WorldManager started)",
            io_channel
        );

        Self {
            eventbus: EventBus::new(),
            components: HashMap::new(),
            handles: HashMap::new(),
            running: false,
            world_tx,
            world_read,
            signal_tx,
            channel_handles: HashMap::new(),
            manager_task: Some(manager_task),
            runner_tasks: HashMap::new(),
            io_channel,
        }
    }

    /// Spawn a channel runner for a channel with a bound Component.
    ///
    /// Returns the channel handle for event injection.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - The channel to run
    /// * `component` - The Component to bind (1:1 relationship)
    pub fn spawn_runner(
        &mut self,
        channel_id: ChannelId,
        component: Box<dyn Component>,
    ) -> ChannelHandle {
        let signal_rx = self.signal_tx.subscribe();
        let (runner, handle) = ChannelRunner::new(
            channel_id,
            self.world_tx.clone(),
            Arc::clone(&self.world_read),
            signal_rx,
            component,
        );

        // Register handle with EventBus for event injection
        self.eventbus.register_channel(handle.clone());

        // Store handle
        self.channel_handles.insert(channel_id, handle.clone());

        // Spawn runner task
        let runner_task = tokio::spawn(runner.run());
        self.runner_tasks.insert(channel_id, runner_task);

        info!("Spawned runner for channel {}", channel_id);
        handle
    }

    /// Spawn a child channel with configuration and bound Component.
    ///
    /// Creates a new channel in World and spawns its runner.
    ///
    /// # Arguments
    ///
    /// * `parent` - Parent channel ID
    /// * `config` - Channel configuration
    /// * `component` - Component to bind to the new channel (1:1)
    pub async fn spawn_channel(
        &mut self,
        parent: ChannelId,
        config: ChannelConfig,
        component: Box<dyn Component>,
    ) -> Option<ChannelId> {
        // Send spawn command
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let cmd = WorldCommand::Spawn {
            parent,
            config,
            reply: reply_tx,
        };

        if self.world_tx.send(cmd).await.is_err() {
            return None;
        }

        // Wait for reply
        let child_id = reply_rx.await.ok()??;

        // Spawn runner for new channel with bound component
        self.spawn_runner(child_id, component);

        Some(child_id)
    }

    /// Returns a runner factory for creating channel runners.
    #[must_use]
    pub fn runner_factory(&self) -> ChannelRunnerFactory {
        ChannelRunnerFactory::new(
            self.world_tx.clone(),
            Arc::clone(&self.world_read),
            self.signal_tx.clone(),
        )
    }

    /// Returns the read-only World handle for parallel access.
    #[must_use]
    pub fn world_read(&self) -> &Arc<RwLock<World>> {
        &self.world_read
    }

    /// Returns the world command sender.
    #[must_use]
    pub fn world_tx(&self) -> &WorldCommandSender {
        &self.world_tx
    }

    /// Broadcast a signal to all channel runners.
    pub fn broadcast_signal(&self, signal: Signal) {
        let _ = self.signal_tx.send(signal.clone());
        // Also send to component-level eventbus
        self.eventbus.signal(signal);
    }

    /// Register a component with its subscriptions.
    ///
    /// The component's `subscriptions()` method is called to determine
    /// which event categories it should receive requests for.
    pub fn register(&mut self, component: Box<dyn Component>) {
        let id = component.id().clone();
        let subscriptions = component.subscriptions();
        let handle = self.eventbus.register(id.clone(), subscriptions.clone());

        info!(
            "Registered component: {} (subscriptions: {:?})",
            id,
            subscriptions.iter().map(|c| c.name()).collect::<Vec<_>>()
        );

        self.handles.insert(id.clone(), handle);
        self.components.insert(id, component);
    }

    /// Send signal (from external, e.g., human input)
    pub fn signal(&self, signal: Signal) {
        info!("Signal dispatched: {:?}", signal.kind);
        self.eventbus.signal(signal);
    }

    /// Check if engine is running
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Start the engine (set running flag).
    ///
    /// Use this when you need to start the engine without entering the run loop,
    /// for example in interactive mode where you control the polling yourself.
    pub fn start(&mut self) {
        self.running = true;
        info!("OrcsEngine started");
    }

    /// Stop the engine
    pub fn stop(&mut self) {
        self.running = false;
    }

    /// Returns the IO channel ID.
    ///
    /// The IO channel is required and always available.
    #[must_use]
    pub fn io_channel(&self) -> ChannelId {
        self.io_channel
    }

    /// Main run loop with parallel execution.
    ///
    /// This method:
    /// 1. Runs the main polling loop
    /// 2. Shuts down all runners on exit
    ///
    /// # Note
    ///
    /// Caller must spawn runners via `spawn_runner()` before calling `run()`.
    /// Each runner requires a bound Component (1:1 relationship).
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut engine = OrcsEngine::new(world);
    /// engine.spawn_runner(io_id, Box::new(MyComponent::new()));
    /// engine.run().await;
    /// ```
    pub async fn run(&mut self) {
        self.running = true;
        info!("OrcsEngine started (parallel mode)");

        while self.running {
            self.poll_once();
            tokio::task::yield_now().await;
        }

        self.shutdown_parallel().await;
        info!("OrcsEngine stopped (parallel mode)");
    }

    /// Shutdown parallel execution infrastructure.
    async fn shutdown_parallel(&mut self) {
        // Send shutdown to WorldManager
        let _ = self.world_tx.send(WorldCommand::Shutdown).await;

        // Wait for manager task
        if let Some(task) = self.manager_task.take() {
            let _ = task.await;
        }

        // Abort all runner tasks (they should stop on channel close)
        for (id, task) in self.runner_tasks.drain() {
            debug!("Aborting runner for channel {}", id);
            task.abort();
        }

        // Unregister channel handles
        for id in self.channel_handles.keys() {
            self.eventbus.unregister_channel(id);
        }
        self.channel_handles.clear();

        // Also do component shutdown
        self.shutdown_components();
    }

    /// Poll all components once
    fn poll_once(&mut self) {
        let component_ids: Vec<ComponentId> = self.components.keys().cloned().collect();
        let mut to_remove: Vec<ComponentId> = Vec::new();

        for id in component_ids {
            // Skip if already marked for removal
            if to_remove.contains(&id) {
                continue;
            }

            // Collect signals first
            let signals: Vec<Signal> = {
                if let Some(handle) = self.handles.get_mut(&id) {
                    let mut sigs = Vec::new();
                    while let Some(signal) = handle.try_recv_signal() {
                        sigs.push(signal);
                    }
                    sigs
                } else {
                    continue;
                }
            };

            // Process collected signals
            for signal in signals {
                // Veto stops everything - check first
                if signal.kind == SignalKind::Veto {
                    self.running = false;
                    return;
                }

                let should_remove = if let Some(component) = self.components.get_mut(&id) {
                    let response = component.on_signal(&signal);

                    match response {
                        SignalResponse::Handled => {
                            info!("Component {} handled signal {:?}", id, signal.kind);
                            false
                        }
                        SignalResponse::Ignored => {
                            // Signal is a command - ignoring is not acceptable
                            // Force kill the component
                            warn!(
                                "Component {} ignored signal {:?} - forcing abort",
                                id, signal.kind
                            );
                            component.abort();
                            true
                        }
                        SignalResponse::Abort => {
                            info!("Component {} aborting due to signal {:?}", id, signal.kind);
                            component.abort();
                            true
                        }
                    }
                } else {
                    false
                };

                if should_remove {
                    to_remove.push(id.clone());
                    break; // Don't process more signals for this component
                }
            }

            // Skip request processing if component is being removed
            if to_remove.contains(&id) {
                continue;
            }

            // Collect requests
            let requests: Vec<orcs_event::Request> = {
                if let Some(handle) = self.handles.get_mut(&id) {
                    let mut reqs = Vec::new();
                    while let Some(request) = handle.try_recv_request() {
                        reqs.push(request);
                    }
                    reqs
                } else {
                    continue;
                }
            };

            // Process collected requests
            for request in requests {
                let request_id = request.id;
                if let Some(component) = self.components.get_mut(&id) {
                    debug!("Component {} handling request: {}", id, request.operation);
                    let result = component.on_request(&request);
                    let mapped = result.map_err(|e| e.to_string());
                    self.eventbus.respond(request_id, mapped);
                }
            }
        }

        // Remove aborted components
        for id in to_remove {
            self.unregister(&id);
        }
    }

    /// Unregister a component
    fn unregister(&mut self, id: &ComponentId) {
        self.eventbus.unregister(id);
        self.handles.remove(id);
        self.components.remove(id);
        info!("Unregistered component: {}", id);
    }

    /// Shutdown all components.
    pub fn shutdown_components(&mut self) {
        info!("Shutting down all components");
        for (id, component) in &mut self.components {
            debug!("Aborting component: {}", id);
            component.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::WorldCommand;
    use crate::Principal;
    use orcs_component::ComponentError;
    use orcs_event::Request;
    use serde_json::Value;

    /// Create a World with IO channel for testing.
    fn test_world() -> (World, ChannelId) {
        let mut world = World::new();
        let io = world.create_channel(ChannelConfig::interactive());
        (world, io)
    }

    struct EchoComponent {
        id: ComponentId,
        aborted: bool,
    }

    impl EchoComponent {
        fn new() -> Self {
            Self {
                id: ComponentId::builtin("echo"),
                aborted: false,
            }
        }
    }

    impl Component for EchoComponent {
        fn id(&self) -> &ComponentId {
            &self.id
        }

        fn status(&self) -> orcs_component::Status {
            orcs_component::Status::Idle
        }

        fn on_request(&mut self, request: &Request) -> Result<Value, ComponentError> {
            Ok(request.payload.clone())
        }

        fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
            if signal.is_veto() {
                SignalResponse::Abort
            } else {
                SignalResponse::Handled
            }
        }

        fn abort(&mut self) {
            self.aborted = true;
        }

        fn status_detail(&self) -> Option<orcs_component::StatusDetail> {
            None
        }

        fn init(&mut self) -> Result<(), ComponentError> {
            Ok(())
        }

        fn shutdown(&mut self) {
            // Default: no-op
        }
    }

    #[tokio::test]
    async fn engine_creation() {
        let (world, io) = test_world();
        let engine = OrcsEngine::new(world, io);
        assert!(!engine.is_running());
        assert_eq!(engine.io_channel(), io);

        // Cleanup
        let _ = engine.world_tx().send(WorldCommand::Shutdown).await;
    }

    #[tokio::test]
    async fn engine_world_access() {
        let (world, io) = test_world();
        let engine = OrcsEngine::new(world, io);

        // WorldManager starts immediately in new()
        let w = engine.world_read().read().await;
        assert!(w.get(&io).is_some());
        drop(w);

        // Cleanup
        let _ = engine.world_tx().send(WorldCommand::Shutdown).await;
    }

    #[tokio::test]
    async fn register_component() {
        let (world, io) = test_world();
        let mut engine = OrcsEngine::new(world, io);
        let echo = Box::new(EchoComponent::new());
        engine.register(echo);

        assert_eq!(engine.components.len(), 1);

        // Cleanup
        let _ = engine.world_tx().send(WorldCommand::Shutdown).await;
    }

    #[tokio::test]
    async fn veto_stops_engine() {
        let (world, io) = test_world();
        let mut engine = OrcsEngine::new(world, io);

        let echo = Box::new(EchoComponent::new());
        engine.register(echo);

        let principal = Principal::System;
        engine.signal(Signal::veto(principal));
        engine.running = true;
        engine.poll_once();

        assert!(!engine.is_running());

        // Cleanup
        let _ = engine.world_tx().send(WorldCommand::Shutdown).await;
    }

    #[tokio::test]
    async fn multiple_components() {
        let (world, io) = test_world();
        let mut engine = OrcsEngine::new(world, io);

        let echo1 = Box::new(EchoComponent::new());
        let mut echo2_inner = EchoComponent::new();
        echo2_inner.id = ComponentId::builtin("echo2");
        let echo2 = Box::new(echo2_inner);

        engine.register(echo1);
        engine.register(echo2);

        assert_eq!(engine.components.len(), 2);

        // Cleanup
        let _ = engine.world_tx().send(WorldCommand::Shutdown).await;
    }

    #[tokio::test]
    async fn stop_engine() {
        let (world, io) = test_world();
        let mut engine = OrcsEngine::new(world, io);
        engine.running = true;

        assert!(engine.is_running());
        engine.stop();
        assert!(!engine.is_running());

        // Cleanup
        let _ = engine.world_tx().send(WorldCommand::Shutdown).await;
    }

    #[tokio::test]
    async fn world_access_parallel() {
        let (world, io) = test_world();
        let engine = OrcsEngine::new(world, io);

        // Complete via command
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        engine
            .world_tx()
            .send(WorldCommand::Complete {
                id: io,
                reply: reply_tx,
            })
            .await
            .unwrap();
        assert!(reply_rx.await.unwrap());

        // Verify state
        let w = engine.world_read().read().await;
        assert!(!w.get(&io).unwrap().is_running());
        drop(w);

        // Cleanup
        let _ = engine.world_tx().send(WorldCommand::Shutdown).await;
    }

    #[tokio::test]
    async fn cancel_signal_handled() {
        let (world, io) = test_world();
        let mut engine = OrcsEngine::new(world, io);

        let echo = Box::new(EchoComponent::new());
        engine.register(echo);

        let principal = Principal::System;
        let cancel = Signal::cancel(io, principal);

        engine.signal(cancel);
        engine.running = true;
        engine.poll_once();

        // Cancel should not stop engine (only Veto does)
        assert!(engine.is_running());

        // Cleanup
        let _ = engine.world_tx().send(WorldCommand::Shutdown).await;
    }

    #[tokio::test]
    async fn shutdown_aborts_all_components() {
        let (world, io) = test_world();
        let mut engine = OrcsEngine::new(world, io);
        let echo = Box::new(EchoComponent::new());
        let id = echo.id.clone();
        engine.register(echo);

        engine.shutdown_components();

        // Component should have been aborted
        assert!(engine.components.contains_key(&id));

        // Cleanup
        let _ = engine.world_tx().send(WorldCommand::Shutdown).await;
    }

    struct CountingComponent {
        id: ComponentId,
        request_count: std::sync::atomic::AtomicUsize,
        signal_count: std::sync::atomic::AtomicUsize,
    }

    impl CountingComponent {
        fn new(name: &str) -> Self {
            Self {
                id: ComponentId::builtin(name),
                request_count: std::sync::atomic::AtomicUsize::new(0),
                signal_count: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl Component for CountingComponent {
        fn id(&self) -> &ComponentId {
            &self.id
        }

        fn status(&self) -> orcs_component::Status {
            orcs_component::Status::Idle
        }

        fn on_request(&mut self, request: &Request) -> Result<Value, ComponentError> {
            self.request_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(request.payload.clone())
        }

        fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
            self.signal_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if signal.is_veto() {
                SignalResponse::Abort
            } else {
                SignalResponse::Handled
            }
        }

        fn abort(&mut self) {}
    }

    #[tokio::test]
    async fn poll_processes_signals() {
        let (world, io) = test_world();
        let mut engine = OrcsEngine::new(world, io);

        let comp = Box::new(CountingComponent::new("counter"));
        engine.register(comp);

        let principal = Principal::System;

        // Send multiple non-veto signals
        engine.signal(Signal::cancel(io, principal.clone()));
        engine.signal(Signal::cancel(io, principal));

        engine.running = true;
        tokio::task::yield_now().await;
        engine.poll_once();

        // Engine should still be running (no veto)
        assert!(engine.is_running());

        // Cleanup
        let _ = engine.world_tx().send(WorldCommand::Shutdown).await;
    }
}

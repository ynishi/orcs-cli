//! OrcsEngine - Main Runtime.
//!
//! The [`OrcsEngine`] is the central runtime that:
//!
//! - Spawns and manages ChannelRunners (parallel execution)
//! - Dispatches Signals to all Runners via broadcast
//! - Coordinates World (Channel hierarchy) management
//! - Provides session persistence via Runner-bound Components
//!
//! # Architecture
//!
//! ```text
//! OrcsEngine
//!     │
//!     ├── signal_tx ────► broadcast to all Runners
//!     │
//!     ├── world_tx ─────► WorldManager (channel operations)
//!     │
//!     ├── channel_handles ──► Event injection to Runners
//!     │
//!     └── runner_components ─► Arc<Mutex<Component>> for snapshot
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
//! Signals are broadcast to all Runners, which deliver them to their
//! bound Components.

use super::eventbus::EventBus;
use crate::board::{self, SharedBoard};
use crate::channel::{
    ChannelConfig, ChannelHandle, ChannelRunner, ClientRunner, ClientRunnerConfig, LuaChildLoader,
    OutputSender, World, WorldCommand, WorldCommandSender, WorldManager,
};
use crate::io::IOPort;
use crate::session::{SessionAsset, SessionStore, StorageError};
use crate::Principal;
use orcs_component::{Component, ComponentSnapshot, SnapshotError};
use orcs_event::Signal;
use orcs_types::ChannelId;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};
use tracing::{debug, info, warn};

/// Signal broadcast channel buffer size.
///
/// 256 signals provides sufficient buffering for HIL interactions.
/// Lagged receivers will miss old signals but continue receiving new ones.
const SIGNAL_BUFFER_SIZE: usize = 256;

/// OrcsEngine - Main runtime for ORCS CLI.
///
/// Coordinates parallel execution of ChannelRunners and manages
/// the World (channel hierarchy).
///
/// # Parallel Execution Model
///
/// Engine operates in parallel mode with:
/// - [`WorldManager`](crate::channel::WorldManager) for concurrent World access
/// - [`ChannelRunner`](crate::channel::ChannelRunner) per channel
/// - Event injection via channel handles
/// - Component access via shared `Arc<Mutex<>>` for snapshots
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
/// Signals are broadcast to all Runners. A Veto signal
/// immediately stops the engine without processing further.
pub struct OrcsEngine {
    /// EventBus for channel event injection
    eventbus: EventBus,
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
    /// Component references for snapshot access.
    ///
    /// Each Runner holds the same Arc, enabling Engine to access
    /// Components for session persistence without owning them.
    runner_components: HashMap<ChannelId, Arc<Mutex<Box<dyn Component>>>>,
    // --- IO Channel (required) ---
    /// IO channel for Human input/output.
    ///
    /// This is a required field - every engine must have an IO channel
    /// for Human interaction.
    io_channel: ChannelId,
    /// Shared Board for cross-component event visibility.
    board: SharedBoard,
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
            running: false,
            world_tx,
            world_read,
            signal_tx,
            channel_handles: HashMap::new(),
            manager_task: Some(manager_task),
            runner_tasks: HashMap::new(),
            runner_components: HashMap::new(),
            io_channel,
            board: board::shared_board(),
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
        let (runner, handle) = ChannelRunner::builder(
            channel_id,
            self.world_tx.clone(),
            Arc::clone(&self.world_read),
            signal_rx,
            component,
        )
        .build();

        // Store Component reference for snapshot access
        let component_ref = Arc::clone(runner.component());
        self.runner_components.insert(channel_id, component_ref);

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

    /// Spawn a channel runner with an EventEmitter injected into the Component.
    ///
    /// This enables IO-less (event-only) execution for Components that support
    /// the Emitter trait. The Component can emit output via `emit_output()`.
    ///
    /// Use this for ChannelRunner-based execution instead of ClientRunner.
    /// The Component receives events via `on_request()` and emits output via Emitter.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - The channel to run
    /// * `component` - The Component to bind (must implement `set_emitter`)
    /// * `output_tx` - Optional sender for routing Output events to IO channel
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Lua scripts can use orcs.output() after emitter is set
    /// let lua_component = LuaComponent::from_script("...")?;
    ///
    /// // Without output routing (Output stays in this channel)
    /// let handle = engine.spawn_runner_with_emitter(channel_id, component, None);
    ///
    /// // With output routing to IO channel
    /// let (_, io_event_tx) = engine.spawn_client_runner(io_channel, io_port, principal);
    /// let handle = engine.spawn_runner_with_emitter(channel_id, component, Some(io_event_tx));
    /// ```
    pub fn spawn_runner_with_emitter(
        &mut self,
        channel_id: ChannelId,
        component: Box<dyn Component>,
        output_tx: Option<OutputSender>,
    ) -> ChannelHandle {
        let signal_rx = self.signal_tx.subscribe();
        let component_id = component.id().clone();

        // Use builder with emitter to ensure event_tx/event_rx consistency
        let mut builder = ChannelRunner::builder(
            channel_id,
            self.world_tx.clone(),
            Arc::clone(&self.world_read),
            signal_rx,
            component,
        )
        .with_emitter(self.signal_tx.clone())
        .with_shared_handles(self.eventbus.shared_handles())
        .with_board(Arc::clone(&self.board));

        // Route Output events to IO channel if specified
        if let Some(tx) = output_tx {
            builder = builder.with_output_channel(tx);
        }

        let (runner, handle) = builder.build();

        // Store Component reference for snapshot access
        let component_ref = Arc::clone(runner.component());
        self.runner_components.insert(channel_id, component_ref);

        // Register handle with EventBus for event injection
        self.eventbus.register_channel(handle.clone());

        // Store handle
        self.channel_handles.insert(channel_id, handle.clone());

        // Spawn runner task
        let runner_task = tokio::spawn(runner.run());
        self.runner_tasks.insert(channel_id, runner_task);

        info!(
            "Spawned runner with emitter for channel {} (component={})",
            channel_id,
            component_id.fqn()
        );
        handle
    }

    /// Spawn a ChannelRunner with all options (emitter, output routing, child spawner).
    ///
    /// This is the most flexible spawn method that supports:
    /// - Event emission (signal broadcasting)
    /// - Output routing to IO channel
    /// - Child spawning via LuaChildLoader
    ///
    /// # Arguments
    ///
    /// * `channel_id` - The channel to run
    /// * `component` - The Component to bind
    /// * `output_tx` - Optional channel for Output event routing
    /// * `lua_loader` - Optional loader for spawning Lua children
    ///
    /// # Returns
    ///
    /// A handle for injecting Events into the Channel.
    pub fn spawn_runner_full(
        &mut self,
        channel_id: ChannelId,
        component: Box<dyn Component>,
        output_tx: Option<OutputSender>,
        lua_loader: Option<Arc<dyn LuaChildLoader>>,
    ) -> ChannelHandle {
        let signal_rx = self.signal_tx.subscribe();
        let component_id = component.id().clone();

        // Use builder with all options
        let mut builder = ChannelRunner::builder(
            channel_id,
            self.world_tx.clone(),
            Arc::clone(&self.world_read),
            signal_rx,
            component,
        )
        .with_emitter(self.signal_tx.clone())
        .with_shared_handles(self.eventbus.shared_handles())
        .with_board(Arc::clone(&self.board));

        // Route Output events to IO channel if specified
        if let Some(tx) = output_tx {
            builder = builder.with_output_channel(tx);
        }

        // Enable child spawning if loader provided
        let has_child_spawner = lua_loader.is_some();
        if has_child_spawner {
            builder = builder.with_child_spawner(lua_loader);
        }

        let (runner, handle) = builder.build();

        // Store Component reference for snapshot access
        let component_ref = Arc::clone(runner.component());
        self.runner_components.insert(channel_id, component_ref);

        // Register handle with EventBus for event injection
        self.eventbus.register_channel(handle.clone());

        // Store handle
        self.channel_handles.insert(channel_id, handle.clone());

        // Spawn runner task
        let runner_task = tokio::spawn(runner.run());
        self.runner_tasks.insert(channel_id, runner_task);

        info!(
            "Spawned runner (full) for channel {} (component={}, child_spawner={})",
            channel_id,
            component_id.fqn(),
            has_child_spawner
        );
        handle
    }

    /// Spawn a ClientRunner for an IO channel (no component).
    ///
    /// ClientRunner is dedicated to Human I/O bridging. It broadcasts UserInput
    /// events to all channels and displays Output events from other channels.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - The channel to run
    /// * `io_port` - IO port for View communication
    /// * `principal` - Principal for signal creation from IO input
    ///
    /// # Returns
    ///
    /// A tuple of (ChannelHandle, event_tx) where event_tx can be used by
    /// other runners to send Output events to this runner for display.
    pub fn spawn_client_runner(
        &mut self,
        channel_id: ChannelId,
        io_port: IOPort,
        principal: orcs_types::Principal,
    ) -> (ChannelHandle, OutputSender) {
        let config = ClientRunnerConfig {
            world_tx: self.world_tx.clone(),
            world: Arc::clone(&self.world_read),
            signal_rx: self.signal_tx.subscribe(),
            channel_handles: self.eventbus.shared_handles(),
        };
        let (runner, handle) = ClientRunner::new(channel_id, config, io_port, principal);

        // Get event_tx for other runners to send Output events (wrapped as OutputSender)
        let output_sender = OutputSender::new(runner.event_tx().clone());

        // Register handle with EventBus for event injection
        self.eventbus.register_channel(handle.clone());

        // Store handle
        self.channel_handles.insert(channel_id, handle.clone());

        // Spawn runner task
        let runner_task = tokio::spawn(runner.run());
        self.runner_tasks.insert(channel_id, runner_task);

        info!("Spawned ClientRunner for channel {}", channel_id);
        (handle, output_sender)
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

    /// Spawn a child channel with authentication/authorization.
    ///
    /// Creates a new channel in World and spawns its runner with
    /// Session and PermissionChecker configured.
    ///
    /// # Authorization Check
    ///
    /// Before spawning, checks if the session is allowed to spawn children
    /// using the provided checker. Returns `None` if unauthorized.
    ///
    /// # Arguments
    ///
    /// * `parent` - Parent channel ID
    /// * `config` - Channel configuration
    /// * `component` - Component to bind to the new channel (1:1)
    /// * `session` - Session for permission checking (Arc for shared grants)
    /// * `checker` - Permission checker policy
    ///
    /// # Returns
    ///
    /// - `Some(ChannelId)` if spawn succeeded
    /// - `None` if parent doesn't exist or permission denied
    pub async fn spawn_channel_with_auth(
        &mut self,
        parent: ChannelId,
        config: ChannelConfig,
        component: Box<dyn Component>,
        session: Arc<crate::Session>,
        checker: Arc<dyn crate::auth::PermissionChecker>,
    ) -> Option<ChannelId> {
        // Permission check: can this session spawn children?
        if !checker.can_spawn_child(&session) {
            warn!(
                principal = ?session.principal(),
                "spawn_channel denied: permission denied"
            );
            return None;
        }

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

        // Spawn runner with auth configured
        self.spawn_runner_with_auth(child_id, component, session, checker);

        Some(child_id)
    }

    /// Spawn a runner with Session and PermissionChecker.
    ///
    /// The Session and Checker are passed to the ChildContext for
    /// command permission checking.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - The channel to run
    /// * `component` - The Component to bind
    /// * `session` - Session for permission checking
    /// * `checker` - Permission checker policy
    pub fn spawn_runner_with_auth(
        &mut self,
        channel_id: ChannelId,
        component: Box<dyn Component>,
        session: Arc<crate::Session>,
        checker: Arc<dyn crate::auth::PermissionChecker>,
    ) -> ChannelHandle {
        let signal_rx = self.signal_tx.subscribe();
        let component_id = component.id().clone();

        // Build runner with auth
        let (runner, handle) = ChannelRunner::builder(
            channel_id,
            self.world_tx.clone(),
            Arc::clone(&self.world_read),
            signal_rx,
            component,
        )
        .with_emitter(self.signal_tx.clone())
        .with_shared_handles(self.eventbus.shared_handles())
        .with_board(Arc::clone(&self.board))
        .with_session_arc(session)
        .with_checker(checker)
        .build();

        // Store Component reference for snapshot access
        let component_ref = Arc::clone(runner.component());
        self.runner_components.insert(channel_id, component_ref);

        // Register handle with EventBus for event injection
        self.eventbus.register_channel(handle.clone());

        // Store handle
        self.channel_handles.insert(channel_id, handle.clone());

        // Spawn runner task
        let runner_task = tokio::spawn(runner.run());
        self.runner_tasks.insert(channel_id, runner_task);

        info!(
            "Spawned runner (with auth) for channel {} (component={})",
            channel_id,
            component_id.fqn(),
        );
        handle
    }

    /// Spawn a ChannelRunner with all options including auth.
    ///
    /// This is the most complete spawn method that supports:
    /// - Event emission (signal broadcasting)
    /// - Output routing to IO channel
    /// - Child spawning via LuaChildLoader
    /// - Session-based permission checking
    ///
    /// # Arguments
    ///
    /// * `channel_id` - The channel to run
    /// * `component` - The Component to bind
    /// * `output_tx` - Optional channel for Output event routing
    /// * `lua_loader` - Optional loader for spawning Lua children
    /// * `session` - Session for permission checking
    /// * `checker` - Permission checker policy
    ///
    /// # Returns
    ///
    /// A handle for injecting Events into the Channel.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_runner_full_auth(
        &mut self,
        channel_id: ChannelId,
        component: Box<dyn Component>,
        output_tx: Option<OutputSender>,
        lua_loader: Option<Arc<dyn LuaChildLoader>>,
        session: Arc<crate::Session>,
        checker: Arc<dyn crate::auth::PermissionChecker>,
        grants: Arc<dyn orcs_auth::GrantPolicy>,
    ) -> ChannelHandle {
        let signal_rx = self.signal_tx.subscribe();
        let component_id = component.id().clone();

        // Use builder with all options including auth
        let mut builder = ChannelRunner::builder(
            channel_id,
            self.world_tx.clone(),
            Arc::clone(&self.world_read),
            signal_rx,
            component,
        )
        .with_emitter(self.signal_tx.clone())
        .with_shared_handles(self.eventbus.shared_handles())
        .with_board(Arc::clone(&self.board))
        .with_session_arc(session)
        .with_checker(checker)
        .with_grants(grants);

        // Route Output events to IO channel if specified
        if let Some(tx) = output_tx {
            builder = builder.with_output_channel(tx);
        }

        // Enable child spawning if loader provided
        let has_child_spawner = lua_loader.is_some();
        if has_child_spawner {
            builder = builder.with_child_spawner(lua_loader);
        }

        let (runner, handle) = builder.build();

        // Store Component reference for snapshot access
        let component_ref = Arc::clone(runner.component());
        self.runner_components.insert(channel_id, component_ref);

        // Register handle with EventBus for event injection
        self.eventbus.register_channel(handle.clone());

        // Store handle
        self.channel_handles.insert(channel_id, handle.clone());

        // Spawn runner task
        let runner_task = tokio::spawn(runner.run());
        self.runner_tasks.insert(channel_id, runner_task);

        info!(
            "Spawned runner (full+auth) for channel {} (component={}, child_spawner={})",
            channel_id,
            component_id.fqn(),
            has_child_spawner
        );
        handle
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

    /// Injects an event to a specific channel (targeted delivery).
    ///
    /// Unlike signal broadcast, this sends to a single channel only.
    /// Used for `@component message` routing.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::ChannelNotFound`] if channel doesn't exist,
    /// or [`EngineError::SendFailed`] if the buffer is full.
    pub fn inject_event(
        &self,
        channel_id: ChannelId,
        event: crate::channel::Event,
    ) -> Result<(), super::EngineError> {
        self.eventbus.try_inject(channel_id, event)
    }

    /// Send signal (from external, e.g., human input)
    ///
    /// Broadcasts to all ChannelRunners via signal_tx.
    pub fn signal(&self, signal: Signal) {
        info!("Signal dispatched: {:?}", signal.kind);
        // Broadcast to ChannelRunners
        let _ = self.signal_tx.send(signal);
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

    /// Stop the engine by sending a Veto signal.
    ///
    /// This triggers graceful shutdown via the signal broadcast channel.
    pub fn stop(&self) {
        info!("Engine stop requested");
        let _ = self.signal_tx.send(Signal::veto(Principal::System));
    }

    /// Returns the shared Board handle.
    ///
    /// External code (e.g., Lua API registration) can use this to
    /// query the Board for recent events.
    #[must_use]
    pub fn board(&self) -> &SharedBoard {
        &self.board
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
    /// 1. Waits for stop signal (running flag set to false)
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
        self.start();
        info!("Entering parallel run loop");

        let mut signal_rx = self.signal_tx.subscribe();

        loop {
            tokio::select! {
                result = signal_rx.recv() => {
                    match result {
                        Ok(signal) if signal.is_veto() => {
                            info!("Veto signal received, stopping engine");
                            break;
                        }
                        Ok(_) => {
                            // Other signals: continue running
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("Signal receiver lagged by {} messages", n);
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            warn!("Signal channel closed unexpectedly");
                            break;
                        }
                    }
                }
            }
        }

        self.running = false;
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

        // Clear component references
        self.runner_components.clear();
    }

    // === Session Persistence ===

    /// Collects snapshots from all Runner-bound Components.
    ///
    /// Iterates over all runner_components and calls `Component::snapshot()`.
    /// Components that don't support snapshots are silently skipped.
    ///
    /// # Returns
    ///
    /// A map of component FQN (fully qualified name) to snapshot.
    pub async fn collect_snapshots(&self) -> HashMap<String, ComponentSnapshot> {
        let mut snapshots = HashMap::new();

        for (channel_id, component_arc) in &self.runner_components {
            let component = component_arc.lock().await;
            let fqn = component.id().fqn();

            match component.snapshot() {
                Ok(snapshot) => {
                    debug!(
                        "Collected snapshot for component {} (channel {})",
                        fqn, channel_id
                    );
                    snapshots.insert(fqn, snapshot);
                }
                Err(SnapshotError::NotSupported(_)) => {
                    debug!("Component {} does not support snapshots", fqn);
                }
                Err(e) => {
                    warn!("Failed to collect snapshot from {}: {}", fqn, e);
                }
            }
        }

        info!("Collected {} component snapshots", snapshots.len());
        snapshots
    }

    /// Restores components from snapshots in a SessionAsset.
    ///
    /// Iterates over all runner_components and restores from matching snapshots.
    ///
    /// # Arguments
    ///
    /// * `asset` - The session asset containing snapshots
    ///
    /// # Returns
    ///
    /// - `Ok(count)` - Number of successfully restored components
    /// - `Err(SnapshotError)` - Restore failed for a component
    pub async fn restore_snapshots(&self, asset: &SessionAsset) -> Result<usize, SnapshotError> {
        let mut restored = 0;

        for component_arc in self.runner_components.values() {
            let mut component = component_arc.lock().await;
            let fqn = component.id().fqn();

            if let Some(snapshot) = asset.component_snapshots.get(&fqn) {
                match component.restore(snapshot) {
                    Ok(()) => {
                        debug!("Restored component: {}", fqn);
                        restored += 1;
                    }
                    Err(SnapshotError::NotSupported(_)) => {
                        warn!("Component {} does not support snapshot restore", fqn);
                    }
                    Err(e) => {
                        return Err(SnapshotError::RestoreFailed {
                            component: fqn,
                            reason: e.to_string(),
                        });
                    }
                }
            }
        }

        info!("Restored {} components from session", restored);
        Ok(restored)
    }

    /// Saves the current session state to a store.
    ///
    /// Collects snapshots from all Runner-bound Components and saves them.
    ///
    /// # Arguments
    ///
    /// * `store` - The session store to save to
    /// * `asset` - The session asset to update and save
    ///
    /// # Errors
    ///
    /// Returns `StorageError` if saving fails.
    pub async fn save_session<S: SessionStore>(
        &self,
        store: &S,
        asset: &mut SessionAsset,
    ) -> Result<(), StorageError> {
        // Collect component snapshots
        let snapshots = self.collect_snapshots().await;

        // Update asset with snapshots
        for (id, snapshot) in snapshots {
            asset.component_snapshots.insert(id, snapshot);
        }
        asset.touch();

        // Save to store
        store.save(asset).await?;

        info!("Session saved: {}", asset.id);
        Ok(())
    }

    /// Loads a session and restores component state.
    ///
    /// Loads the session from the store and restores all component
    /// snapshots to their Runner-bound Components.
    ///
    /// # Arguments
    ///
    /// * `store` - The session store to load from
    /// * `session_id` - The session ID to load
    ///
    /// # Returns
    ///
    /// The loaded SessionAsset with restored component states.
    ///
    /// # Errors
    ///
    /// - `StorageError::Storage` - Loading from store failed
    /// - `StorageError::Snapshot` - Restore failed for a component
    pub async fn load_session<S: SessionStore>(
        &self,
        store: &S,
        session_id: &str,
    ) -> Result<SessionAsset, StorageError> {
        // Load session from store
        let asset = store.load(session_id).await?;

        // Restore component snapshots
        let restored = self.restore_snapshots(&asset).await?;

        info!(
            "Session loaded: {} ({} components restored)",
            session_id, restored
        );
        Ok(asset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{ChannelCore, WorldCommand};
    use crate::Principal;
    use orcs_component::ComponentError;
    use orcs_event::{Request, SignalResponse};
    use orcs_types::ComponentId;
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
    async fn stop_engine_sends_veto_signal() {
        let (world, io) = test_world();
        let engine = OrcsEngine::new(world, io);

        // Subscribe before stop
        let mut rx = engine.signal_tx.subscribe();

        // stop() should send Veto signal
        engine.stop();

        // Verify Veto was sent
        let received = rx.recv().await.expect("receive signal");
        assert!(received.is_veto());

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
            .expect("send complete command");
        assert!(reply_rx.await.expect("receive complete reply"));

        // Verify state
        let w = engine.world_read().read().await;
        assert!(!w.get(&io).expect("get channel").is_running());
        drop(w);

        // Cleanup
        let _ = engine.world_tx().send(WorldCommand::Shutdown).await;
    }

    #[tokio::test]
    async fn spawn_runner_stores_component_ref() {
        let (world, io) = test_world();
        let mut engine = OrcsEngine::new(world, io);

        let echo = Box::new(EchoComponent::new());
        let _handle = engine.spawn_runner(io, echo);

        // Component should be stored in runner_components
        assert_eq!(engine.runner_components.len(), 1);
        assert!(engine.runner_components.contains_key(&io));

        // Cleanup
        let _ = engine.world_tx().send(WorldCommand::Shutdown).await;
    }

    #[tokio::test]
    async fn signal_broadcast_works() {
        let (world, io) = test_world();
        let engine = OrcsEngine::new(world, io);

        // Subscribe before signal
        let mut rx = engine.signal_tx.subscribe();

        let principal = Principal::System;
        let cancel = Signal::cancel(io, principal);
        engine.signal(cancel.clone());

        // Should receive the signal
        let received = rx.recv().await.expect("receive signal");
        assert!(matches!(received.kind, orcs_event::SignalKind::Cancel));

        // Cleanup
        let _ = engine.world_tx().send(WorldCommand::Shutdown).await;
    }

    // === Snapshot Tests ===

    /// A component that supports snapshots for testing.
    struct SnapshottableComponent {
        id: ComponentId,
        counter: u64,
    }

    impl SnapshottableComponent {
        fn new(name: &str, initial_value: u64) -> Self {
            Self {
                id: ComponentId::builtin(name),
                counter: initial_value,
            }
        }
    }

    impl Component for SnapshottableComponent {
        fn id(&self) -> &ComponentId {
            &self.id
        }

        fn status(&self) -> orcs_component::Status {
            orcs_component::Status::Idle
        }

        fn on_request(&mut self, _request: &Request) -> Result<Value, ComponentError> {
            self.counter += 1;
            Ok(Value::Number(self.counter.into()))
        }

        fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
            if signal.is_veto() {
                SignalResponse::Abort
            } else {
                SignalResponse::Handled
            }
        }

        fn abort(&mut self) {}

        fn snapshot(
            &self,
        ) -> Result<orcs_component::ComponentSnapshot, orcs_component::SnapshotError> {
            orcs_component::ComponentSnapshot::from_state(self.id.fqn(), &self.counter)
        }

        fn restore(
            &mut self,
            snapshot: &orcs_component::ComponentSnapshot,
        ) -> Result<(), orcs_component::SnapshotError> {
            self.counter = snapshot.to_state()?;
            Ok(())
        }
    }

    #[tokio::test]
    async fn collect_snapshots_from_runner_components() {
        let (world, io) = test_world();
        let mut engine = OrcsEngine::new(world, io);

        // Spawn runner with snapshottable component
        let comp = Box::new(SnapshottableComponent::new("snap", 42));
        let _handle = engine.spawn_runner(io, comp);

        // Collect snapshots (async now)
        let snapshots = engine.collect_snapshots().await;

        // Should have one snapshot
        assert_eq!(snapshots.len(), 1);
        assert!(snapshots.contains_key("builtin::snap"));

        // Cleanup
        let _ = engine.world_tx().send(WorldCommand::Shutdown).await;
    }

    #[tokio::test]
    async fn restore_snapshots_to_runner_components() {
        let (world, io) = test_world();
        let mut engine = OrcsEngine::new(world, io);

        // Spawn runner with snapshottable component (initial value 0)
        let comp = Box::new(SnapshottableComponent::new("snap", 0));
        let _handle = engine.spawn_runner(io, comp);

        // Create asset with snapshot containing different value
        let mut asset = SessionAsset::new();
        let snapshot = orcs_component::ComponentSnapshot::from_state("builtin::snap", &100u64)
            .expect("create snapshot");
        asset.save_snapshot(snapshot);

        // Restore from asset
        let restored = engine
            .restore_snapshots(&asset)
            .await
            .expect("restore snapshots");

        assert_eq!(restored, 1);

        // Cleanup
        let _ = engine.world_tx().send(WorldCommand::Shutdown).await;
    }

    #[tokio::test]
    async fn save_and_load_session_via_runners() {
        use crate::session::LocalFileStore;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().expect("create temp dir");
        let store = LocalFileStore::new(temp_dir.path().to_path_buf()).expect("create store");

        let (world, io) = test_world();
        let mut engine = OrcsEngine::new(world, io);

        // Spawn runner with snapshottable component
        let comp = Box::new(SnapshottableComponent::new("snap", 42));
        let _handle = engine.spawn_runner(io, comp);

        // Save session
        let mut asset = SessionAsset::new();
        let session_id = asset.id.clone();
        engine
            .save_session(&store, &mut asset)
            .await
            .expect("save session");

        // Verify snapshot was saved (uses FQN format)
        assert!(asset.get_snapshot("builtin::snap").is_some());

        // Create new engine and load session
        let (world2, io2) = test_world();
        let mut engine2 = OrcsEngine::new(world2, io2);

        // Spawn runner with component (different initial value)
        let comp2 = Box::new(SnapshottableComponent::new("snap", 0));
        let _handle2 = engine2.spawn_runner(io2, comp2);

        // Load session
        let loaded = engine2
            .load_session(&store, &session_id)
            .await
            .expect("load session");

        assert_eq!(loaded.id, session_id);
        assert!(loaded.get_snapshot("builtin::snap").is_some());

        // Cleanup
        let _ = engine.world_tx().send(WorldCommand::Shutdown).await;
        let _ = engine2.world_tx().send(WorldCommand::Shutdown).await;
    }

    // === spawn_channel_with_auth Tests ===

    mod spawn_channel_with_auth_tests {
        use super::*;
        use crate::auth::{DefaultPolicy, Session};
        use orcs_types::PrincipalId;
        use std::time::Duration;

        fn standard_session() -> Arc<Session> {
            Arc::new(Session::new(Principal::User(PrincipalId::new())))
        }

        fn elevated_session() -> Arc<Session> {
            Arc::new(
                Session::new(Principal::User(PrincipalId::new())).elevate(Duration::from_secs(60)),
            )
        }

        fn default_checker() -> Arc<dyn crate::auth::PermissionChecker> {
            Arc::new(DefaultPolicy)
        }

        #[tokio::test]
        async fn spawn_channel_with_auth_denied_for_standard_session() {
            let (world, io) = test_world();
            let mut engine = OrcsEngine::new(world, io);

            let session = standard_session();
            let checker = default_checker();
            let config = ChannelConfig::new(100, true);
            let component = Box::new(EchoComponent::new());

            // Standard session should be denied
            let result = engine
                .spawn_channel_with_auth(io, config, component, session, checker)
                .await;

            assert!(result.is_none(), "standard session should be denied");

            // Cleanup
            let _ = engine.world_tx().send(WorldCommand::Shutdown).await;
        }

        #[tokio::test]
        async fn spawn_channel_with_auth_allowed_for_elevated_session() {
            let (world, io) = test_world();
            let mut engine = OrcsEngine::new(world, io);

            let session = elevated_session();
            let checker = default_checker();
            let config = ChannelConfig::new(100, true);
            let component = Box::new(EchoComponent::new());

            // Elevated session should be allowed
            let result = engine
                .spawn_channel_with_auth(io, config, component, session, checker)
                .await;

            assert!(result.is_some(), "elevated session should be allowed");

            let child_id = result.unwrap();
            // Verify child was created in world
            let w = engine.world_read().read().await;
            assert!(w.get(&child_id).is_some());
            drop(w);

            // Cleanup
            let _ = engine.world_tx().send(WorldCommand::Shutdown).await;
        }

        #[tokio::test]
        async fn spawn_runner_with_auth_creates_runner() {
            let (world, io) = test_world();
            let mut engine = OrcsEngine::new(world, io);

            let session = elevated_session();
            let checker = default_checker();
            let component = Box::new(EchoComponent::new());

            let handle = engine.spawn_runner_with_auth(io, component, session, checker);

            assert_eq!(handle.id, io);
            assert!(engine.runner_components.contains_key(&io));

            // Cleanup
            let _ = engine.world_tx().send(WorldCommand::Shutdown).await;
        }
    }
}

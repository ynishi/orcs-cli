//! OrcsEngine - Main Runtime.
//!
//! The [`OrcsEngine`] is the central runtime that:
//!
//! - Spawns and manages ChannelRunners (parallel execution)
//! - Dispatches Signals to all Runners via broadcast
//! - Coordinates World (Channel hierarchy) management
//! - Provides session persistence via graceful shutdown snapshots
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
//!     └── collected_snapshots ─► Snapshots from graceful shutdown
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
    OutputSender, RunnerResult, World, WorldCommand, WorldCommandSender, WorldManager,
};
use crate::io::IOPort;
use crate::session::{SessionAsset, SessionStore, StorageError};
use crate::Principal;
use orcs_component::{Component, ComponentSnapshot};
use orcs_event::Signal;
use orcs_hook::SharedHookRegistry;
use orcs_types::{ChannelId, ComponentId};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, info, warn};

/// Signal broadcast channel buffer size.
///
/// 256 signals provides sufficient buffering for HIL interactions.
/// Lagged receivers will miss old signals but continue receiving new ones.
const SIGNAL_BUFFER_SIZE: usize = 256;

/// Timeout for waiting on each runner task during graceful shutdown.
const RUNNER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

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
/// - Snapshot collection via graceful runner shutdown
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
    /// Channel runner task handles.
    ///
    /// Each runner returns a [`RunnerResult`] containing the component's
    /// shutdown snapshot. The Engine awaits these on shutdown instead of
    /// aborting them.
    runner_tasks: HashMap<ChannelId, tokio::task::JoinHandle<RunnerResult>>,
    /// Snapshots collected from runners during graceful shutdown.
    ///
    /// Populated by [`shutdown_parallel()`] when runners complete.
    /// Keyed by component FQN (fully qualified name).
    collected_snapshots: HashMap<String, ComponentSnapshot>,
    // --- IO Channel (required) ---
    /// IO channel for Human input/output.
    ///
    /// This is a required field - every engine must have an IO channel
    /// for Human interaction.
    io_channel: ChannelId,
    /// Shared Board for cross-component event visibility.
    board: SharedBoard,
    /// Shared hook registry (injected into all spawned runners).
    hook_registry: Option<SharedHookRegistry>,
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
            collected_snapshots: HashMap::new(),
            io_channel,
            board: board::shared_board(),
            hook_registry: None,
        }
    }

    /// Sets the shared hook registry for all spawned runners.
    ///
    /// Must be called before spawning runners to ensure they receive
    /// the registry.
    pub fn set_hook_registry(&mut self, registry: SharedHookRegistry) {
        self.eventbus.set_hook_registry(Arc::clone(&registry));
        self.hook_registry = Some(registry);
    }

    /// Returns a reference to the hook registry, if configured.
    #[must_use]
    pub fn hook_registry(&self) -> Option<&SharedHookRegistry> {
        self.hook_registry.as_ref()
    }

    /// Applies the hook registry to a ChannelRunnerBuilder if configured.
    fn apply_hook_registry(
        &self,
        builder: crate::channel::ChannelRunnerBuilder,
    ) -> crate::channel::ChannelRunnerBuilder {
        match &self.hook_registry {
            Some(reg) => builder.with_hook_registry(Arc::clone(reg)),
            None => builder,
        }
    }

    /// Registers runner with EventBus, stores handle, and spawns the tokio task.
    ///
    /// Common finalization extracted from `spawn_runner*` methods.
    fn finalize_runner(
        &mut self,
        channel_id: ChannelId,
        component_id: &ComponentId,
        runner: ChannelRunner,
        handle: ChannelHandle,
    ) -> ChannelHandle {
        self.eventbus.register_channel(handle.clone());
        self.eventbus
            .register_component_channel(component_id, channel_id);
        self.channel_handles.insert(channel_id, handle.clone());
        let runner_task = tokio::spawn(runner.run());
        self.runner_tasks.insert(channel_id, runner_task);
        handle
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
        let component_id = component.id().clone();
        let signal_rx = self.signal_tx.subscribe();
        let builder = ChannelRunner::builder(
            channel_id,
            self.world_tx.clone(),
            Arc::clone(&self.world_read),
            signal_rx,
            component,
        )
        .with_request_channel();
        let (runner, handle) = self.apply_hook_registry(builder).build();

        let handle = self.finalize_runner(channel_id, &component_id, runner, handle);
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
        .with_component_channel_map(self.eventbus.shared_component_channel_map())
        .with_board(Arc::clone(&self.board))
        .with_request_channel();

        // Route Output events to IO channel if specified
        if let Some(tx) = output_tx {
            builder = builder.with_output_channel(tx);
        }

        let (runner, handle) = self.apply_hook_registry(builder).build();

        let handle = self.finalize_runner(channel_id, &component_id, runner, handle);
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
        .with_component_channel_map(self.eventbus.shared_component_channel_map())
        .with_board(Arc::clone(&self.board))
        .with_request_channel();

        // Route Output events to IO channel if specified
        if let Some(tx) = output_tx {
            builder = builder.with_output_channel(tx);
        }

        // Enable child spawning if loader provided
        let has_child_spawner = lua_loader.is_some();
        if has_child_spawner {
            builder = builder.with_child_spawner(lua_loader);
        }

        let (runner, handle) = self.apply_hook_registry(builder).build();

        let handle = self.finalize_runner(channel_id, &component_id, runner, handle);
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
        let builder = ChannelRunner::builder(
            channel_id,
            self.world_tx.clone(),
            Arc::clone(&self.world_read),
            signal_rx,
            component,
        )
        .with_emitter(self.signal_tx.clone())
        .with_shared_handles(self.eventbus.shared_handles())
        .with_component_channel_map(self.eventbus.shared_component_channel_map())
        .with_board(Arc::clone(&self.board))
        .with_session_arc(session)
        .with_checker(checker)
        .with_request_channel();
        let (runner, handle) = self.apply_hook_registry(builder).build();

        let handle = self.finalize_runner(channel_id, &component_id, runner, handle);
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
        .with_component_channel_map(self.eventbus.shared_component_channel_map())
        .with_board(Arc::clone(&self.board))
        .with_session_arc(session)
        .with_checker(checker)
        .with_grants(grants)
        .with_request_channel();

        // Route Output events to IO channel if specified
        if let Some(tx) = output_tx {
            builder = builder.with_output_channel(tx);
        }

        // Enable child spawning if loader provided
        let has_child_spawner = lua_loader.is_some();
        if has_child_spawner {
            builder = builder.with_child_spawner(lua_loader);
        }

        let (runner, handle) = self.apply_hook_registry(builder).build();

        let handle = self.finalize_runner(channel_id, &component_id, runner, handle);
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

    /// Shutdown parallel execution infrastructure (graceful).
    ///
    /// # Shutdown Order
    ///
    /// 1. Await all runner tasks with timeout (Veto already broadcast).
    ///    Runners execute their shutdown sequence (snapshot → shutdown)
    ///    and return [`RunnerResult`] with captured snapshots.
    /// 2. Collect snapshots from completed runners into `collected_snapshots`.
    /// 3. Shutdown WorldManager (runners may need `world_tx` during shutdown,
    ///    so WorldManager must stay alive until runners complete).
    /// 4. Unregister channel handles and clean up.
    async fn shutdown_parallel(&mut self) {
        // 1. Await all runner tasks in parallel with per-runner timeout.
        //    Each runner is wrapped in a tokio task that applies the timeout
        //    and aborts on expiry. All wrappers run concurrently, so total
        //    wall time is bounded by RUNNER_SHUTDOWN_TIMEOUT (not N × timeout).
        let tasks: Vec<_> = self.runner_tasks.drain().collect();
        let mut wrapper_handles = Vec::with_capacity(tasks.len());

        for (id, mut task) in tasks {
            wrapper_handles.push(tokio::spawn(async move {
                tokio::select! {
                    result = &mut task => {
                        match result {
                            Ok(runner_result) => Some((id, runner_result)),
                            Err(e) => {
                                warn!(channel = %id, error = %e, "runner task panicked");
                                None
                            }
                        }
                    }
                    _ = tokio::time::sleep(RUNNER_SHUTDOWN_TIMEOUT) => {
                        warn!(channel = %id, "runner task timed out, aborting");
                        task.abort();
                        None
                    }
                }
            }));
        }

        // 2. Collect snapshots from completed runners.
        for handle in wrapper_handles {
            if let Ok(Some((id, result))) = handle.await {
                debug!(
                    channel = %id,
                    component = %result.component_fqn,
                    has_snapshot = result.snapshot.is_some(),
                    "runner completed gracefully"
                );
                if let Some(snapshot) = result.snapshot {
                    self.collected_snapshots
                        .insert(result.component_fqn.into_owned(), snapshot);
                }
            }
        }

        info!(
            "collected {} snapshots from runners",
            self.collected_snapshots.len()
        );

        // 3. Shutdown WorldManager (after runners complete)
        let _ = self.world_tx.send(WorldCommand::Shutdown).await;
        if let Some(task) = self.manager_task.take() {
            let _ = task.await;
        }

        // 4. Unregister channel handles
        for id in self.channel_handles.keys() {
            self.eventbus.unregister_channel(id);
        }
        self.channel_handles.clear();
    }

    // === Session Persistence ===

    /// Returns a reference to snapshots collected during graceful shutdown.
    ///
    /// Populated by [`shutdown_parallel()`] when runners complete their
    /// shutdown sequence. Keyed by component FQN.
    ///
    /// # Usage
    ///
    /// Call this after `run()` completes to retrieve snapshots for session
    /// persistence.
    #[must_use]
    pub fn collected_snapshots(&self) -> &HashMap<String, ComponentSnapshot> {
        &self.collected_snapshots
    }

    /// Saves the current session state to a store.
    ///
    /// Uses snapshots collected during graceful shutdown (populated by
    /// `shutdown_parallel()`). Must be called after `run()` completes.
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
        // Write collected snapshots into asset
        for (fqn, snapshot) in &self.collected_snapshots {
            asset
                .component_snapshots
                .insert(fqn.clone(), snapshot.clone());
        }
        asset.touch();

        // Save to store
        store.save(asset).await?;

        info!(
            "Session saved: {} ({} snapshots)",
            asset.id,
            self.collected_snapshots.len()
        );
        Ok(())
    }

    /// Loads a session asset from a store.
    ///
    /// Returns the loaded asset containing component snapshots. The caller
    /// is responsible for passing these snapshots to
    /// [`ChannelRunnerBuilder::with_initial_snapshot()`] when spawning runners.
    ///
    /// # Arguments
    ///
    /// * `store` - The session store to load from
    /// * `session_id` - The session ID to load
    ///
    /// # Errors
    ///
    /// Returns `StorageError` if loading fails.
    pub async fn load_session<S: SessionStore>(
        &self,
        store: &S,
        session_id: &str,
    ) -> Result<SessionAsset, StorageError> {
        let asset = store.load(session_id).await?;

        info!(
            "Session loaded: {} ({} snapshots available)",
            session_id,
            asset.component_snapshots.len()
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
    async fn spawn_runner_creates_task() {
        let (world, io) = test_world();
        let mut engine = OrcsEngine::new(world, io);

        let echo = Box::new(EchoComponent::new());
        let _handle = engine.spawn_runner(io, echo);

        // Runner task should be stored
        assert_eq!(engine.runner_tasks.len(), 1);
        assert!(engine.runner_tasks.contains_key(&io));

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
    async fn graceful_shutdown_collects_snapshots() {
        let (world, io) = test_world();
        let mut engine = OrcsEngine::new(world, io);

        // Spawn runner with snapshottable component
        let comp = Box::new(SnapshottableComponent::new("snap", 42));
        let _handle = engine.spawn_runner(io, comp);

        // Run engine briefly then stop via Veto
        let engine_signal_tx = engine.signal_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let _ = engine_signal_tx.send(Signal::veto(Principal::System));
        });

        engine.run().await;

        // Snapshots should have been collected during graceful shutdown
        let snapshots = engine.collected_snapshots();
        assert_eq!(snapshots.len(), 1);
        assert!(snapshots.contains_key("builtin::snap"));
    }

    #[tokio::test]
    async fn save_session_after_graceful_shutdown() {
        use crate::session::LocalFileStore;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().expect("create temp dir");
        let store = LocalFileStore::new(temp_dir.path().to_path_buf()).expect("create store");

        let (world, io) = test_world();
        let mut engine = OrcsEngine::new(world, io);

        // Spawn runner with snapshottable component
        let comp = Box::new(SnapshottableComponent::new("snap", 42));
        let _handle = engine.spawn_runner(io, comp);

        // Run engine briefly then stop via Veto
        let engine_signal_tx = engine.signal_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let _ = engine_signal_tx.send(Signal::veto(Principal::System));
        });

        engine.run().await;

        // Save session (uses collected snapshots from shutdown)
        let mut asset = SessionAsset::new();
        let session_id = asset.id.clone();
        engine
            .save_session(&store, &mut asset)
            .await
            .expect("save session");

        // Verify snapshot was saved
        assert!(asset.get_snapshot("builtin::snap").is_some());

        // Load session from a new engine
        let (world2, io2) = test_world();
        let engine2 = OrcsEngine::new(world2, io2);

        let loaded = engine2
            .load_session(&store, &session_id)
            .await
            .expect("load session");

        assert_eq!(loaded.id, session_id);
        assert!(loaded.get_snapshot("builtin::snap").is_some());

        // Cleanup
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
            assert!(engine.runner_tasks.contains_key(&io));

            // Cleanup
            let _ = engine.world_tx().send(WorldCommand::Shutdown).await;
        }
    }
}

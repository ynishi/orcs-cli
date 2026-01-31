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

use super::error::EngineError;
use super::eventbus::{ComponentHandle, EventBus};
use crate::channel::{
    ChannelConfig, ChannelHandle, ChannelRunner, ChannelRunnerFactory, World, WorldCommand,
    WorldCommandSender, WorldManager,
};
use crate::session::{SessionAsset, SessionStore, StorageError};
use orcs_component::{Component, ComponentSnapshot, EventCategory, Package, PackageInfo};
use orcs_event::{Request, Signal, SignalKind, SignalResponse};
use orcs_types::{ChannelId, ComponentId};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, error, info, warn};

/// Signal broadcast channel buffer size.
///
/// 256 signals provides sufficient buffering for HIL interactions.
/// Lagged receivers will miss old signals but continue receiving new ones.
const SIGNAL_BUFFER_SIZE: usize = 256;

/// Lifecycle operation: snapshot collection.
const OP_SNAPSHOT: &str = "snapshot";

/// Lifecycle operation: state restoration.
const OP_RESTORE: &str = "restore";

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

    // === Session Persistence ===

    /// Collects snapshots from all components via Lifecycle Request.
    ///
    /// Sends `Request { category: Lifecycle, operation: "snapshot" }` to each component.
    /// Components that support snapshots return `Ok(Value)` containing the snapshot.
    /// Components that don't support snapshots return `Err(NotSupported)` and are skipped.
    ///
    /// Returns a map of component FQN (fully qualified name) to snapshot.
    /// Uses FQN (`namespace::name`) instead of full ID to enable
    /// snapshot restoration across sessions (UUIDs differ between runs).
    pub fn collect_snapshots(&mut self) -> HashMap<String, ComponentSnapshot> {
        let mut snapshots = HashMap::new();

        // Collect component IDs first to avoid borrow issues
        let component_ids: Vec<ComponentId> = self.components.keys().cloned().collect();

        for id in component_ids {
            if let Some(component) = self.components.get_mut(&id) {
                let request = Request::new(
                    EventCategory::Lifecycle,
                    OP_SNAPSHOT,
                    id.clone(),
                    ChannelId::new(),
                    serde_json::Value::Null,
                );

                match component.on_request(&request) {
                    Ok(value) => {
                        // Deserialize snapshot from Value
                        match serde_json::from_value::<ComponentSnapshot>(value) {
                            Ok(snapshot) => {
                                debug!("Collected snapshot for component: {}", id.fqn());
                                snapshots.insert(id.fqn(), snapshot);
                            }
                            Err(e) => {
                                warn!("Failed to deserialize snapshot from {}: {}", id.fqn(), e);
                            }
                        }
                    }
                    Err(orcs_component::ComponentError::NotSupported(_)) => {
                        // Component doesn't support snapshots - skip silently
                        debug!("Component {} does not support snapshots", id.fqn());
                    }
                    Err(e) => {
                        warn!("Failed to collect snapshot from {}: {}", id.fqn(), e);
                    }
                }
            }
        }

        info!("Collected {} component snapshots", snapshots.len());
        snapshots
    }

    /// Restores components from snapshots in a SessionAsset via Lifecycle Request.
    ///
    /// Sends `Request { category: Lifecycle, operation: "restore", payload: snapshot }`
    /// to each component that has a saved snapshot.
    ///
    /// # Returns
    ///
    /// - `Ok(count)` - Number of successfully restored components
    /// - `Err(SnapshotError)` - Restore failed for a component
    pub fn restore_snapshots(
        &mut self,
        asset: &SessionAsset,
    ) -> Result<usize, orcs_component::SnapshotError> {
        let mut restored = 0;

        for (fqn, snapshot) in &asset.component_snapshots {
            // Find component by FQN (namespace::name)
            let component_id = self.components.keys().find(|k| k.fqn() == *fqn).cloned();

            if let Some(id) = component_id {
                if let Some(component) = self.components.get_mut(&id) {
                    // Serialize snapshot to Value for request payload
                    let payload = match serde_json::to_value(snapshot) {
                        Ok(v) => v,
                        Err(e) => {
                            return Err(orcs_component::SnapshotError::RestoreFailed {
                                component: fqn.clone(),
                                reason: format!("Failed to serialize snapshot: {e}"),
                            });
                        }
                    };

                    let request = Request::new(
                        EventCategory::Lifecycle,
                        OP_RESTORE,
                        id.clone(),
                        ChannelId::new(),
                        payload,
                    );

                    match component.on_request(&request) {
                        Ok(_) => {
                            debug!("Restored component: {}", id.fqn());
                            restored += 1;
                        }
                        Err(orcs_component::ComponentError::NotSupported(_)) => {
                            warn!("Component {} does not support snapshot restore", id.fqn());
                        }
                        Err(e) => {
                            error!("Failed to restore component {}: {}", id.fqn(), e);
                            return Err(orcs_component::SnapshotError::RestoreFailed {
                                component: id.fqn(),
                                reason: e.to_string(),
                            });
                        }
                    }
                }
            } else {
                debug!(
                    "Component {} not registered, skipping snapshot restore",
                    fqn
                );
            }
        }

        info!("Restored {} components from session", restored);
        Ok(restored)
    }

    /// Saves the current session state to a store.
    ///
    /// Collects snapshots from all components and saves them along with
    /// the provided SessionAsset.
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
        &mut self,
        store: &S,
        asset: &mut SessionAsset,
    ) -> Result<(), StorageError> {
        // Collect component snapshots
        let snapshots = self.collect_snapshots();

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
    /// snapshots. For components with `SnapshotSupport::Enabled`,
    /// restore failures are errors (not warnings).
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
    /// - `StorageError::Snapshot` - Restore failed for Enabled component
    pub async fn load_session<S: SessionStore>(
        &mut self,
        store: &S,
        session_id: &str,
    ) -> Result<SessionAsset, StorageError> {
        // Load session from store
        let asset = store.load(session_id).await?;

        // Restore component snapshots
        let restored = self.restore_snapshots(&asset)?;

        info!(
            "Session loaded: {} ({} components restored)",
            session_id, restored
        );
        Ok(asset)
    }

    // --- Package Management ---

    /// Collects package info from all registered components.
    ///
    /// Only components that implement [`Packageable`] will report packages.
    ///
    /// # Returns
    ///
    /// A map of component ID to list of installed packages.
    pub fn collect_packages(&self) -> HashMap<ComponentId, Vec<PackageInfo>> {
        let mut packages = HashMap::new();

        for (id, component) in &self.components {
            if let Some(packageable) = component.as_packageable() {
                let list = packageable.list_packages();
                if !list.is_empty() {
                    packages.insert(id.clone(), list);
                }
            }
        }

        packages
    }

    /// Installs a package into a specific component.
    ///
    /// The component must implement [`Packageable`].
    ///
    /// # Arguments
    ///
    /// * `component_id` - The target component
    /// * `package` - The package to install
    ///
    /// # Errors
    ///
    /// - `EngineError::ComponentNotFound` - Component doesn't exist
    /// - `EngineError::PackageNotSupported` - Component doesn't support packages
    /// - `EngineError::PackageFailed` - Installation failed
    pub fn install_package(
        &mut self,
        component_id: &ComponentId,
        package: &Package,
    ) -> Result<(), EngineError> {
        let component = self
            .components
            .get_mut(component_id)
            .ok_or_else(|| EngineError::ComponentNotFound(component_id.clone()))?;

        let packageable = component
            .as_packageable_mut()
            .ok_or_else(|| EngineError::PackageNotSupported(component_id.clone()))?;

        packageable
            .install_package(package)
            .map_err(|e| EngineError::PackageFailed(e.to_string()))?;

        info!(
            "Package '{}' installed to component '{}'",
            package.id(),
            component_id
        );

        Ok(())
    }

    /// Uninstalls a package from a specific component.
    ///
    /// # Arguments
    ///
    /// * `component_id` - The target component
    /// * `package_id` - The package ID to uninstall
    ///
    /// # Errors
    ///
    /// - `EngineError::ComponentNotFound` - Component doesn't exist
    /// - `EngineError::PackageNotSupported` - Component doesn't support packages
    /// - `EngineError::PackageFailed` - Uninstallation failed
    pub fn uninstall_package(
        &mut self,
        component_id: &ComponentId,
        package_id: &str,
    ) -> Result<(), EngineError> {
        let component = self
            .components
            .get_mut(component_id)
            .ok_or_else(|| EngineError::ComponentNotFound(component_id.clone()))?;

        let packageable = component
            .as_packageable_mut()
            .ok_or_else(|| EngineError::PackageNotSupported(component_id.clone()))?;

        packageable
            .uninstall_package(package_id)
            .map_err(|e| EngineError::PackageFailed(e.to_string()))?;

        info!(
            "Package '{}' uninstalled from component '{}'",
            package_id, component_id
        );

        Ok(())
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
            match request.operation.as_str() {
                OP_SNAPSHOT | OP_RESTORE => {
                    Err(ComponentError::NotSupported(request.operation.clone()))
                }
                _ => Ok(request.payload.clone()),
            }
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

    // === Snapshot Tests ===

    /// A component that supports snapshots for testing via Request.
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

        fn on_request(&mut self, request: &Request) -> Result<Value, ComponentError> {
            match request.operation.as_str() {
                OP_SNAPSHOT => {
                    // Return snapshot as Value
                    let snapshot =
                        orcs_component::ComponentSnapshot::from_state(self.id.fqn(), &self.counter)
                            .map_err(|e| ComponentError::ExecutionFailed(e.to_string()))?;
                    serde_json::to_value(snapshot)
                        .map_err(|e| ComponentError::ExecutionFailed(e.to_string()))
                }
                OP_RESTORE => {
                    // Restore from snapshot in payload
                    let snapshot: orcs_component::ComponentSnapshot =
                        serde_json::from_value(request.payload.clone())
                            .map_err(|e| ComponentError::ExecutionFailed(e.to_string()))?;
                    self.counter = snapshot
                        .to_state()
                        .map_err(|e| ComponentError::ExecutionFailed(e.to_string()))?;
                    Ok(Value::Null)
                }
                _ => {
                    self.counter += 1;
                    Ok(Value::Number(self.counter.into()))
                }
            }
        }

        fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
            if signal.is_veto() {
                SignalResponse::Abort
            } else {
                SignalResponse::Handled
            }
        }

        fn abort(&mut self) {}
    }

    #[tokio::test]
    async fn collect_snapshots_from_components() {
        let (world, io) = test_world();
        let mut engine = OrcsEngine::new(world, io);

        // Register snapshottable component
        let comp = Box::new(SnapshottableComponent::new("snap", 42));
        engine.register(comp);

        // Register non-snapshottable component
        let echo = Box::new(EchoComponent::new());
        engine.register(echo);

        // Collect snapshots
        let snapshots = engine.collect_snapshots();

        // Only snapshottable component should have snapshot
        assert_eq!(snapshots.len(), 1);
        assert!(snapshots.contains_key("builtin::snap"));

        // Cleanup
        let _ = engine.world_tx().send(WorldCommand::Shutdown).await;
    }

    #[tokio::test]
    async fn restore_snapshots_to_components() {
        let (world, io) = test_world();
        let mut engine = OrcsEngine::new(world, io);

        // Register snapshottable component with initial value
        let comp = Box::new(SnapshottableComponent::new("snap", 0));
        engine.register(comp);

        // Create asset with snapshot containing different value
        // Use FQN format (namespace::name) for snapshot key
        let mut asset = SessionAsset::new();
        let snapshot = orcs_component::ComponentSnapshot::from_state("builtin::snap", &100u64)
            .expect("create snapshot");
        asset.save_snapshot(snapshot);

        // Restore from asset
        let restored = engine.restore_snapshots(&asset).expect("restore snapshots");

        assert_eq!(restored, 1);

        // Cleanup
        let _ = engine.world_tx().send(WorldCommand::Shutdown).await;
    }

    #[tokio::test]
    async fn save_and_load_session() {
        use crate::session::LocalFileStore;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().expect("create temp dir");
        let store = LocalFileStore::new(temp_dir.path().to_path_buf()).expect("create store");

        let (world, io) = test_world();
        let mut engine = OrcsEngine::new(world, io);

        // Register snapshottable component
        let comp = Box::new(SnapshottableComponent::new("snap", 42));
        engine.register(comp);

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

        // Register component with different initial value
        let comp2 = Box::new(SnapshottableComponent::new("snap", 0));
        engine2.register(comp2);

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
}

//! ChannelRunner - Parallel execution context for a Channel.
//!
//! Each [`ChannelRunner`] runs in its own tokio task, enabling true
//! parallel execution of Channels. It receives Events via mpsc and
//! sends World modifications via the command queue.
//!
//! # Architecture
//!
//! ```text
//!                           ┌──────────────────────────┐
//!                           │     ChannelRunner        │
//!                           │                          │
//! EventBus ───inject()────► │  event_rx ◄── mpsc       │
//!                           │                          │
//! Human ────signal()──────► │  signal_rx ◄── broadcast │
//!                           │                          │
//!                           │         │                │
//!                           │         ▼                │
//!                           │  process_event()         │
//!                           │         │                │
//!                           │         ▼                │
//!                           │  world_tx ───► WorldManager
//!                           │                          │
//!                           └──────────────────────────┘
//! ```
//!
//! # Lifecycle
//!
//! 1. Created via [`ChannelRunner::new()`]
//! 2. Started with [`ChannelRunner::run()`] (spawns tokio task)
//! 3. Processes Events until channel completes or is killed
//! 4. Cleanup on drop

use super::common::{
    determine_channel_action, dispatch_signal_to_component, is_channel_active, is_channel_paused,
    send_abort, send_transition, SignalAction,
};
use crate::channel::command::{StateTransition, WorldCommand};
use crate::channel::config::ChannelConfig;
use crate::channel::World;
use orcs_component::Component;
use orcs_event::{EventCategory, Request, Signal};
use orcs_types::ChannelId;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use tracing::{debug, info, warn};

/// An event that can be injected into a channel.
///
/// Events are the primary means of external input to a running channel.
/// Unlike Signals (which are control messages), Events represent work
/// to be processed by the bound Component.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Category of this event (for routing).
    pub category: EventCategory,
    /// Operation to perform (maps to Component.on_request operation).
    pub operation: String,
    /// Source component that sent this event.
    pub source: orcs_types::ComponentId,
    /// Event payload data.
    pub payload: serde_json::Value,
}

/// Default event buffer size per channel.
const EVENT_BUFFER_SIZE: usize = 64;

/// Maximum queue size for events received while paused.
const PAUSED_QUEUE_MAX_SIZE: usize = 128;

/// Handle for injecting events into a [`ChannelRunner`].
#[derive(Clone, Debug)]
pub struct ChannelHandle {
    /// Channel ID.
    pub id: ChannelId,
    /// Event sender.
    event_tx: mpsc::Sender<Event>,
}

impl ChannelHandle {
    /// Creates a new handle with the given sender.
    #[must_use]
    pub fn new(id: ChannelId, event_tx: mpsc::Sender<Event>) -> Self {
        Self { id, event_tx }
    }

    /// Injects an event into the channel.
    ///
    /// Returns an error if the channel has been dropped.
    pub async fn inject(&self, event: Event) -> Result<(), mpsc::error::SendError<Event>> {
        self.event_tx.send(event).await
    }

    /// Try to inject an event without blocking.
    ///
    /// Returns an error if the buffer is full or channel is dropped.
    #[allow(clippy::result_large_err)] // Event included in error for retry
    pub fn try_inject(&self, event: Event) -> Result<(), mpsc::error::TrySendError<Event>> {
        self.event_tx.try_send(event)
    }
}

/// Execution context for a single Channel.
///
/// `ChannelRunner` provides the runtime environment for parallel
/// channel execution. Each runner:
///
/// - Binds 1:1 with a [`Component`]
/// - Receives Events via an mpsc channel and delivers to Component
/// - Receives Signals via broadcast and delivers to Component
/// - Sends World modifications via command queue
/// - Has read access to the World for queries
pub struct ChannelRunner {
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
    paused_queue: VecDeque<Event>,
}

impl ChannelRunner {
    /// Creates a new ChannelRunner with a bound Component.
    ///
    /// Returns the runner and a handle for injecting events.
    #[must_use]
    pub fn new(
        id: ChannelId,
        world_tx: mpsc::Sender<WorldCommand>,
        world: Arc<RwLock<World>>,
        signal_rx: broadcast::Receiver<Signal>,
        component: Box<dyn Component>,
    ) -> (Self, ChannelHandle) {
        let (event_tx, event_rx) = mpsc::channel(EVENT_BUFFER_SIZE);

        let runner = Self {
            id,
            event_rx,
            signal_rx,
            world_tx,
            world,
            component: Arc::new(Mutex::new(component)),
            paused_queue: VecDeque::new(),
        };

        let handle = ChannelHandle::new(id, event_tx);

        (runner, handle)
    }

    /// Returns a reference to the bound Component.
    #[must_use]
    pub fn component(&self) -> &Arc<Mutex<Box<dyn Component>>> {
        &self.component
    }

    /// Returns this channel's ID.
    #[must_use]
    pub fn id(&self) -> ChannelId {
        self.id
    }

    /// Returns a reference to the world_tx sender.
    #[must_use]
    pub fn world_tx(&self) -> &mpsc::Sender<WorldCommand> {
        &self.world_tx
    }

    /// Returns a reference to the world.
    #[must_use]
    pub fn world(&self) -> &Arc<RwLock<World>> {
        &self.world
    }

    /// Runs the channel's event loop.
    ///
    /// This method consumes the runner and processes events until:
    /// - The channel is completed or killed
    /// - A Veto signal is received
    /// - All event senders are dropped
    pub async fn run(mut self) {
        info!("ChannelRunner {} started", self.id);

        loop {
            tokio::select! {
                // Priority: signals first
                biased;

                signal = self.signal_rx.recv() => {
                    match signal {
                        Ok(sig) => {
                            if !self.handle_signal(sig).await {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            info!("ChannelRunner {}: signal channel closed", self.id);
                            break;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("ChannelRunner {}: lagged {} signals", self.id, n);
                        }
                    }
                }

                event = self.event_rx.recv() => {
                    match event {
                        Some(evt) => {
                            if !self.handle_event(evt).await {
                                break;
                            }
                        }
                        None => {
                            info!("ChannelRunner {}: event channel closed", self.id);
                            break;
                        }
                    }
                }
            }

            // Check if channel is still running
            if !is_channel_active(&self.world, self.id).await {
                debug!("ChannelRunner {}: channel no longer active", self.id);
                break;
            }
        }

        info!("ChannelRunner {} stopped", self.id);
    }

    /// Handles an incoming signal.
    ///
    /// Returns `false` if the runner should stop.
    async fn handle_signal(&mut self, signal: Signal) -> bool {
        debug!(
            "ChannelRunner {}: received signal {:?}",
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
                "ChannelRunner {}: component requested stop: {}",
                self.id, reason
            );
            send_abort(&self.world_tx, self.id, &reason).await;
            return false;
        }

        // Determine channel-level action
        let action = determine_channel_action(&signal.kind);
        match action {
            SignalAction::Stop { reason } => {
                info!("ChannelRunner {}: stopping - {}", self.id, reason);
                send_abort(&self.world_tx, self.id, &reason).await;
                return false;
            }
            SignalAction::Transition(transition) => {
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
    /// If paused, queues the event for later processing.
    async fn handle_event(&mut self, event: Event) -> bool {
        debug!(
            "ChannelRunner {}: received event {:?} op={}",
            self.id, event.category, event.operation
        );

        // Queue events while paused
        if is_channel_paused(&self.world, self.id).await {
            if self.paused_queue.len() >= PAUSED_QUEUE_MAX_SIZE {
                warn!(
                    "ChannelRunner {}: paused queue full (max={}), dropping event",
                    self.id, PAUSED_QUEUE_MAX_SIZE
                );
                return true;
            }
            debug!(
                "ChannelRunner {}: queuing event while paused (queue_size={})",
                self.id,
                self.paused_queue.len() + 1
            );
            self.paused_queue.push_back(event);
            return true;
        }

        self.process_event(event).await;
        true
    }

    /// Processes a single event by delivering it to the Component.
    async fn process_event(&self, event: Event) {
        let request = Request::new(
            event.category,
            &event.operation,
            event.source,
            self.id,
            event.payload,
        );

        let result = {
            let mut comp = self.component.lock().await;
            comp.on_request(&request)
        };

        match result {
            Ok(response) => {
                debug!(
                    "ChannelRunner {}: Component returned success: {:?}",
                    self.id, response
                );
            }
            Err(e) => {
                warn!("ChannelRunner {}: Component returned error: {}", self.id, e);
            }
        }
    }

    /// Drains the paused queue and processes all queued events.
    async fn drain_paused_queue(&mut self) {
        if self.paused_queue.is_empty() {
            return;
        }

        let count = self.paused_queue.len();
        info!(
            "ChannelRunner {}: draining {} queued events after resume",
            self.id, count
        );

        while let Some(event) = self.paused_queue.pop_front() {
            self.process_event(event).await;
        }
    }

    /// Spawns a child channel with a bound Component.
    ///
    /// Returns the new channel's ID and handle, or None if spawn failed.
    pub async fn spawn_child(
        &self,
        config: ChannelConfig,
        signal_rx: broadcast::Receiver<Signal>,
        component: Box<dyn Component>,
    ) -> Option<(ChannelRunner, ChannelHandle)> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let cmd = WorldCommand::Spawn {
            parent: self.id,
            config,
            reply: reply_tx,
        };

        if self.world_tx.send(cmd).await.is_err() {
            return None;
        }

        let child_id = reply_rx.await.ok()??;
        let (runner, handle) = ChannelRunner::new(
            child_id,
            self.world_tx.clone(),
            Arc::clone(&self.world),
            signal_rx,
            component,
        );

        Some((runner, handle))
    }
}

/// Factory for creating channel runners.
pub struct ChannelRunnerFactory {
    /// Command sender for World modifications.
    world_tx: mpsc::Sender<WorldCommand>,
    /// Read-only World access.
    world: Arc<RwLock<World>>,
    /// Signal broadcaster.
    signal_tx: broadcast::Sender<Signal>,
}

impl ChannelRunnerFactory {
    /// Creates a new factory.
    #[must_use]
    pub fn new(
        world_tx: mpsc::Sender<WorldCommand>,
        world: Arc<RwLock<World>>,
        signal_tx: broadcast::Sender<Signal>,
    ) -> Self {
        Self {
            world_tx,
            world,
            signal_tx,
        }
    }

    /// Creates a runner for an existing channel with a bound Component.
    #[must_use]
    pub fn create(
        &self,
        id: ChannelId,
        component: Box<dyn Component>,
    ) -> (ChannelRunner, ChannelHandle) {
        let signal_rx = self.signal_tx.subscribe();
        ChannelRunner::new(
            id,
            self.world_tx.clone(),
            Arc::clone(&self.world),
            signal_rx,
            component,
        )
    }

    /// Broadcasts a signal to all runners.
    pub fn signal(&self, signal: Signal) {
        let _ = self.signal_tx.send(signal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::manager::WorldManager;
    use crate::channel::ChannelConfig;
    use orcs_component::{ComponentError, Status};
    use orcs_event::{EventCategory, SignalResponse};
    use orcs_types::{ComponentId, Principal};
    use serde_json::Value;

    /// Test mock component that echoes requests.
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

    #[tokio::test]
    async fn runner_creation() {
        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let signal_rx = signal_tx.subscribe();
        let (runner, handle) = ChannelRunner::new(
            primary,
            world_tx.clone(),
            world,
            signal_rx,
            mock_component(),
        );

        assert_eq!(runner.id(), primary);
        assert_eq!(handle.id, primary);

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn runner_receives_events() {
        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let signal_rx = signal_tx.subscribe();
        let (runner, handle) = ChannelRunner::new(
            primary,
            world_tx.clone(),
            world,
            signal_rx,
            mock_component(),
        );

        let runner_task = tokio::spawn(runner.run());

        let event = Event {
            category: EventCategory::Echo,
            operation: "echo".to_string(),
            source: ComponentId::builtin("test"),
            payload: serde_json::json!({"test": true}),
        };
        handle.inject(event).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        signal_tx
            .send(Signal::cancel(primary, Principal::System))
            .unwrap();

        let _ = tokio::time::timeout(std::time::Duration::from_millis(100), runner_task).await;

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn runner_handles_veto() {
        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let signal_rx = signal_tx.subscribe();
        let (runner, _handle) = ChannelRunner::new(
            primary,
            world_tx.clone(),
            world.clone(),
            signal_rx,
            mock_component(),
        );

        let runner_task = tokio::spawn(runner.run());

        tokio::task::yield_now().await;

        signal_tx.send(Signal::veto(Principal::System)).unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_millis(100), runner_task).await;
        assert!(result.is_ok());

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn channel_handle_clone() {
        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let signal_rx = signal_tx.subscribe();
        let (_runner, handle) = ChannelRunner::new(
            primary,
            world_tx.clone(),
            world,
            signal_rx,
            mock_component(),
        );

        let handle2 = handle.clone();
        assert_eq!(handle.id, handle2.id);

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn factory_creates_runners() {
        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let factory = ChannelRunnerFactory::new(world_tx.clone(), world, signal_tx.clone());

        let (runner, handle) = factory.create(primary, mock_component());
        assert_eq!(runner.id(), primary);
        assert_eq!(handle.id, primary);

        teardown(manager_task, world_tx).await;
    }
}

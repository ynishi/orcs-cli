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
//! 1. Created via [`ChannelRunner::builder()`]
//! 2. Started with [`ChannelRunner::run()`] (spawns tokio task)
//! 3. Processes Events until channel completes or is killed
//! 4. Cleanup on drop

use super::child_context::{ChildContextImpl, LuaChildLoader};
use super::child_spawner::ChildSpawner;
use super::common::{
    determine_channel_action, dispatch_signal_to_component, is_channel_active, is_channel_paused,
    send_abort, send_transition, SignalAction,
};
use super::paused_queue::PausedEventQueue;
use super::EventEmitter;
use crate::auth::{PermissionChecker, Session};
use crate::channel::command::{StateTransition, WorldCommand};
use crate::channel::config::ChannelConfig;
use crate::channel::World;
use crate::engine::SharedChannelHandles;
use orcs_component::{
    AsyncChildContext, ChildContext, Component, ComponentLoader, ComponentSnapshot, SnapshotError,
};
use orcs_event::{EventCategory, Request, Signal};
use orcs_types::ChannelId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::borrow::Cow;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, RwLock};
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

/// Internal routing wrapper that distinguishes broadcast from direct event injection.
///
/// - [`Broadcast`](InboundEvent::Broadcast): Events sent to all channels
///   (e.g., UserInput from ClientRunner). The subscription filter is applied —
///   only channels subscribed to the event's category will process it.
/// - [`Direct`](InboundEvent::Direct): Events targeted at a specific channel
///   (e.g., `@component` routing via Engine). The subscription filter is
///   bypassed — the channel always processes the event.
///
/// This enum is internal to the runner system and does not affect the
/// [`Event`] struct itself.
#[derive(Debug, Clone)]
pub(crate) enum InboundEvent {
    /// Broadcast event — subscription filter applies.
    Broadcast(Event),
    /// Direct event — subscription filter bypassed.
    Direct(Event),
}

impl InboundEvent {
    /// Extracts the inner [`Event`], consuming the wrapper.
    pub(crate) fn into_event(self) -> Event {
        match self {
            Self::Broadcast(e) | Self::Direct(e) => e,
        }
    }

    /// Returns `true` if this is a direct (filter-bypassing) event.
    pub(crate) fn is_direct(&self) -> bool {
        matches!(self, Self::Direct(_))
    }
}

/// Opaque handle for sending events into a channel as [`InboundEvent::Direct`].
///
/// All events sent through `OutputSender` bypass the subscription filter
/// on the receiving [`ChannelRunner`]. Use [`OutputSender::channel()`] to
/// create a matched sender/receiver pair for testing or external use.
///
/// Internally wraps `mpsc::Sender<InboundEvent>` without exposing
/// the `InboundEvent` type to external crates.
#[derive(Clone, Debug)]
pub struct OutputSender {
    inner: mpsc::Sender<InboundEvent>,
}

impl OutputSender {
    /// Creates a matched (`OutputSender`, [`OutputReceiver`]) pair.
    ///
    /// This is the public way to create a channel for use with
    /// [`ChildSpawner`] and [`ChildContextImpl`] in integration tests
    /// or external code.
    #[must_use]
    pub fn channel(buffer: usize) -> (Self, OutputReceiver) {
        let (tx, rx) = mpsc::channel(buffer);
        (Self { inner: tx }, OutputReceiver { inner: rx })
    }

    /// Creates a new OutputSender from an InboundEvent sender (crate-internal).
    pub(crate) fn new(tx: mpsc::Sender<InboundEvent>) -> Self {
        Self { inner: tx }
    }

    /// Returns the inner sender (crate-internal).
    #[allow(dead_code)]
    pub(crate) fn into_inner(self) -> mpsc::Sender<InboundEvent> {
        self.inner
    }

    /// Sends an event as [`InboundEvent::Direct`] (non-blocking).
    #[allow(clippy::result_large_err)]
    pub(crate) fn try_send_direct(
        &self,
        event: Event,
    ) -> Result<(), mpsc::error::TrySendError<InboundEvent>> {
        self.inner.try_send(InboundEvent::Direct(event))
    }

    /// Sends an event as [`InboundEvent::Direct`] (async, waits for capacity).
    #[allow(dead_code)]
    pub(crate) async fn send_direct(
        &self,
        event: Event,
    ) -> Result<(), mpsc::error::SendError<InboundEvent>> {
        self.inner.send(InboundEvent::Direct(event)).await
    }
}

/// Receiver end of an [`OutputSender`] channel.
///
/// Unwraps [`InboundEvent`] internally, returning plain [`Event`] values.
/// Created via [`OutputSender::channel()`].
pub struct OutputReceiver {
    inner: mpsc::Receiver<InboundEvent>,
}

impl OutputReceiver {
    /// Receives the next event, waiting until one is available.
    ///
    /// Returns `None` when all senders have been dropped.
    pub async fn recv(&mut self) -> Option<Event> {
        self.inner.recv().await.map(InboundEvent::into_event)
    }

    /// Attempts to receive an event without blocking.
    pub fn try_recv(&mut self) -> Result<Event, mpsc::error::TryRecvError> {
        self.inner.try_recv().map(InboundEvent::into_event)
    }
}

/// Result of a ChannelRunner's execution.
///
/// Returned by [`ChannelRunner::run()`] after the event loop exits and
/// the shutdown sequence completes. Contains the channel ID, component
/// FQN, and an optional snapshot captured during graceful shutdown.
#[derive(Debug)]
pub struct RunnerResult {
    /// Channel ID this runner was bound to.
    pub channel_id: ChannelId,
    /// Fully qualified name of the bound Component.
    ///
    /// `Cow::Borrowed` for static FQNs (e.g. ClientRunner),
    /// `Cow::Owned` for dynamic FQNs (e.g. ChannelRunner components).
    pub component_fqn: Cow<'static, str>,
    /// Snapshot captured during shutdown (None if component doesn't support snapshots).
    pub snapshot: Option<ComponentSnapshot>,
}

/// An RPC request paired with its reply channel.
///
/// Created by [`EventBus::request()`] and delivered to [`ChannelRunner`]
/// via its `request_rx` channel. The runner calls `Component::on_request()`
/// and sends the result back through `reply_tx`.
pub(crate) struct RequestEnvelope {
    /// The incoming request.
    pub request: Request,
    /// One-shot channel to send the response back to the caller.
    pub reply_tx: oneshot::Sender<Result<Value, String>>,
}

/// Buffer size for the request channel (Component-to-Component RPC).
const REQUEST_BUFFER_SIZE: usize = 32;

/// Default event buffer size per channel.
const EVENT_BUFFER_SIZE: usize = 64;

/// Handle for injecting events into a [`ChannelRunner`].
#[derive(Clone, Debug)]
pub struct ChannelHandle {
    /// Channel ID.
    pub id: ChannelId,
    /// Event sender (carries [`InboundEvent`] internally).
    event_tx: mpsc::Sender<InboundEvent>,
    /// Request sender for Component-to-Component RPC (None if not enabled).
    request_tx: Option<mpsc::Sender<RequestEnvelope>>,
}

impl ChannelHandle {
    /// Creates a new handle with the given sender.
    #[must_use]
    pub(crate) fn new(id: ChannelId, event_tx: mpsc::Sender<InboundEvent>) -> Self {
        Self {
            id,
            event_tx,
            request_tx: None,
        }
    }

    /// Returns `true` if this handle accepts RPC requests.
    #[must_use]
    pub fn accepts_requests(&self) -> bool {
        self.request_tx.is_some()
    }

    /// Sends an RPC request to the bound Component.
    ///
    /// The request is delivered to the ChannelRunner's `request_rx` and
    /// processed by `Component::on_request()`. The result is sent back
    /// through `reply_tx`.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the request channel is not enabled or is closed.
    pub(crate) async fn send_request(
        &self,
        request: Request,
        reply_tx: oneshot::Sender<Result<Value, String>>,
    ) -> Result<(), mpsc::error::SendError<RequestEnvelope>> {
        match &self.request_tx {
            Some(tx) => tx.send(RequestEnvelope { request, reply_tx }).await,
            None => Err(mpsc::error::SendError(RequestEnvelope {
                request,
                reply_tx,
            })),
        }
    }

    /// Injects a broadcast event into the channel (subscription filter applies).
    ///
    /// Returns an error if the channel has been dropped.
    pub(crate) async fn inject(
        &self,
        event: Event,
    ) -> Result<(), mpsc::error::SendError<InboundEvent>> {
        self.event_tx.send(InboundEvent::Broadcast(event)).await
    }

    /// Try to inject a broadcast event without blocking.
    ///
    /// Returns an error if the buffer is full or channel is dropped.
    #[allow(clippy::result_large_err)]
    pub(crate) fn try_inject(
        &self,
        event: Event,
    ) -> Result<(), mpsc::error::TrySendError<InboundEvent>> {
        self.event_tx.try_send(InboundEvent::Broadcast(event))
    }

    /// Injects a direct event into the channel (subscription filter bypassed).
    ///
    /// Use this for targeted delivery (e.g., `@component` routing).
    pub(crate) async fn inject_direct(
        &self,
        event: Event,
    ) -> Result<(), mpsc::error::SendError<InboundEvent>> {
        self.event_tx.send(InboundEvent::Direct(event)).await
    }

    /// Try to inject a direct event without blocking.
    ///
    /// Bypasses the subscription filter on the receiving ChannelRunner.
    #[allow(clippy::result_large_err)]
    pub(crate) fn try_inject_direct(
        &self,
        event: Event,
    ) -> Result<(), mpsc::error::TrySendError<InboundEvent>> {
        self.event_tx.try_send(InboundEvent::Direct(event))
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
/// - Manages spawned children via ChildSpawner
pub struct ChannelRunner {
    /// This channel's ID.
    id: ChannelId,
    /// Receiver for incoming events (wrapped as [`InboundEvent`]).
    event_rx: mpsc::Receiver<InboundEvent>,
    /// Receiver for signals (broadcast).
    signal_rx: broadcast::Receiver<Signal>,
    /// Sender for World commands.
    world_tx: mpsc::Sender<WorldCommand>,
    /// Read-only World access.
    world: Arc<RwLock<World>>,
    /// Bound Component (1:1 relationship).
    component: Arc<Mutex<Box<dyn Component>>>,
    /// Cached event subscriptions from Component.
    ///
    /// Populated at build time to avoid locking Component on every event.
    /// Broadcast events whose category is not in this list are silently skipped.
    /// Direct events bypass this filter entirely.
    subscriptions: Vec<EventCategory>,
    /// Queue for events received while paused.
    paused_queue: PausedEventQueue,
    /// Child spawner for managing spawned children (optional).
    child_spawner: Option<Arc<StdMutex<ChildSpawner>>>,
    /// Event sender for child context (kept for context creation).
    event_tx: Option<mpsc::Sender<InboundEvent>>,
    /// Receiver for incoming RPC requests from other Components.
    ///
    /// When enabled via [`ChannelRunnerBuilder::with_request_channel()`],
    /// the runner accepts [`RequestEnvelope`]s, calls `Component::on_request()`,
    /// and sends the result back through the envelope's `reply_tx`.
    request_rx: Option<mpsc::Receiver<RequestEnvelope>>,
    /// Initial snapshot to restore before init (session resume).
    initial_snapshot: Option<ComponentSnapshot>,
}

/// Helper for `tokio::select!`: receives from an optional request channel.
///
/// Returns `None` immediately if the receiver is `None` (request channel
/// not enabled), allowing the select! branch to be skipped.
async fn recv_request(rx: &mut Option<mpsc::Receiver<RequestEnvelope>>) -> Option<RequestEnvelope> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

impl ChannelRunner {
    /// Creates a ChildContext for use by managed children.
    ///
    /// The returned context can be injected into LuaChild instances
    /// to enable them to spawn sub-children.
    ///
    /// Returns None if child spawning was not enabled for this runner.
    #[must_use]
    pub fn create_child_context(&self, child_id: &str) -> Option<Box<dyn ChildContext>> {
        let spawner = self.child_spawner.as_ref()?;
        let event_tx = self.event_tx.as_ref()?;

        let ctx = ChildContextImpl::new(
            child_id,
            OutputSender::new(event_tx.clone()),
            Arc::clone(spawner),
        );

        Some(Box::new(ctx))
    }

    /// Creates a ChildContext with a LuaChildLoader for spawning Lua children.
    ///
    /// # Arguments
    ///
    /// * `child_id` - ID of the child that will use this context
    /// * `loader` - Loader for creating LuaChild instances from configs
    #[must_use]
    pub fn create_child_context_with_loader(
        &self,
        child_id: &str,
        loader: Arc<dyn LuaChildLoader>,
    ) -> Option<Box<dyn ChildContext>> {
        let spawner = self.child_spawner.as_ref()?;
        let event_tx = self.event_tx.as_ref()?;

        let ctx = ChildContextImpl::new(
            child_id,
            OutputSender::new(event_tx.clone()),
            Arc::clone(spawner),
        )
        .with_lua_loader(loader);

        Some(Box::new(ctx))
    }

    /// Creates an AsyncChildContext for use by async children.
    ///
    /// The returned context can be used with async spawn operations.
    ///
    /// Returns None if child spawning was not enabled for this runner.
    #[must_use]
    pub fn create_async_child_context(&self, child_id: &str) -> Option<Box<dyn AsyncChildContext>> {
        let spawner = self.child_spawner.as_ref()?;
        let event_tx = self.event_tx.as_ref()?;

        let ctx = ChildContextImpl::new(
            child_id,
            OutputSender::new(event_tx.clone()),
            Arc::clone(spawner),
        );

        Some(Box::new(ctx))
    }

    /// Creates an AsyncChildContext with a LuaChildLoader.
    ///
    /// # Arguments
    ///
    /// * `child_id` - ID of the child that will use this context
    /// * `loader` - Loader for creating LuaChild instances from configs
    #[must_use]
    pub fn create_async_child_context_with_loader(
        &self,
        child_id: &str,
        loader: Arc<dyn LuaChildLoader>,
    ) -> Option<Box<dyn AsyncChildContext>> {
        let spawner = self.child_spawner.as_ref()?;
        let event_tx = self.event_tx.as_ref()?;

        let ctx = ChildContextImpl::new(
            child_id,
            OutputSender::new(event_tx.clone()),
            Arc::clone(spawner),
        )
        .with_lua_loader(loader);

        Some(Box::new(ctx))
    }

    /// Returns a reference to the child spawner, if enabled.
    #[must_use]
    pub fn child_spawner(&self) -> Option<&Arc<StdMutex<ChildSpawner>>> {
        self.child_spawner.as_ref()
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
    ///
    /// After the event loop exits, executes the shutdown sequence:
    /// 1. Capture component snapshot (if supported)
    /// 2. Call `component.shutdown()` for cleanup
    /// 3. Return [`RunnerResult`] with snapshot
    ///
    /// If an initial snapshot was provided via
    /// [`ChannelRunnerBuilder::with_initial_snapshot()`], it is restored
    /// before `init()` is called.
    #[tracing::instrument(skip_all, level = "info", fields(channel_id = %self.id))]
    pub async fn run(mut self) -> RunnerResult {
        info!("ChannelRunner started");

        // Restore + Initialize component
        {
            let mut comp = self.component.lock().await;

            // Restore from initial snapshot (session resume)
            if let Some(snapshot) = self.initial_snapshot.take() {
                match comp.restore(&snapshot) {
                    Ok(()) => info!("restored component from initial snapshot"),
                    Err(SnapshotError::NotSupported(_)) => {
                        debug!("component does not support snapshot restore");
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to restore initial snapshot");
                    }
                }
            }

            if let Err(e) = comp.init() {
                warn!(error = %e, "component init failed");
            }
        }

        loop {
            tokio::select! {
                // Priority: signals > requests > events
                biased;

                signal = self.signal_rx.recv() => {
                    match signal {
                        Ok(sig) => {
                            if !self.handle_signal(sig).await {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            info!("signal channel closed");
                            break;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!(lagged = n, "signal receiver lagged");
                        }
                    }
                }

                // RPC requests from other Components (higher priority than events).
                // When request_rx is None, recv_request() returns pending (never resolves),
                // effectively disabling this branch in select!.
                Some(envelope) = recv_request(&mut self.request_rx) => {
                    self.handle_rpc_request(envelope).await;
                }

                event = self.event_rx.recv() => {
                    match event {
                        Some(evt) => {
                            if !self.handle_event(evt).await {
                                break;
                            }
                        }
                        None => {
                            info!("event channel closed");
                            break;
                        }
                    }
                }
            }

            // Check if channel is still running
            if !is_channel_active(&self.world, self.id).await {
                debug!("channel no longer active");
                break;
            }
        }

        // === Shutdown Sequence ===
        let (component_fqn, snapshot) = {
            let mut comp = self.component.lock().await;
            let fqn = comp.id().fqn();

            // 1. Capture snapshot
            let snapshot = match comp.snapshot() {
                Ok(s) => {
                    debug!(component = %fqn, "captured shutdown snapshot");
                    Some(s)
                }
                Err(SnapshotError::NotSupported(_)) => None,
                Err(e) => {
                    warn!(component = %fqn, error = %e, "snapshot failed during shutdown");
                    None
                }
            };

            // 2. Shutdown
            comp.shutdown();
            debug!(component = %fqn, "component shutdown complete");

            (fqn, snapshot)
        };

        info!("ChannelRunner stopped");

        RunnerResult {
            channel_id: self.id,
            component_fqn: Cow::Owned(component_fqn),
            snapshot,
        }
    }

    /// Handles an incoming signal.
    ///
    /// Returns `false` if the runner should stop.
    async fn handle_signal(&mut self, signal: Signal) -> bool {
        debug!(signal_kind = ?signal.kind, "received signal");

        // Check if signal affects this channel
        if !signal.affects_channel(self.id) {
            return true;
        }

        // Propagate signal to spawned children
        if let Some(spawner) = &self.child_spawner {
            if let Ok(mut s) = spawner.lock() {
                s.propagate_signal(&signal);
            }
        }

        // Dispatch to component first
        let component_action = dispatch_signal_to_component(&signal, &self.component).await;
        if let SignalAction::Stop { reason } = component_action {
            info!(reason = %reason, "component requested stop");
            // Abort all children before stopping
            self.abort_all_children();
            send_abort(&self.world_tx, self.id, &reason).await;
            return false;
        }

        // Determine channel-level action
        let action = determine_channel_action(&signal.kind);
        match action {
            SignalAction::Stop { reason } => {
                info!(reason = %reason, "stopping channel");
                // Abort all children before stopping
                self.abort_all_children();
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

    /// Aborts all spawned children.
    fn abort_all_children(&self) {
        if let Some(spawner) = &self.child_spawner {
            if let Ok(mut s) = spawner.lock() {
                s.abort_all();
                debug!("aborted all children");
            }
        }
    }

    /// Handles an incoming event.
    ///
    /// Subscription filter is applied first (for broadcast events),
    /// then paused events are queued for later processing.
    /// Direct events bypass the subscription filter entirely.
    async fn handle_event(&mut self, inbound: InboundEvent) -> bool {
        let is_direct = inbound.is_direct();
        let event = inbound.into_event();

        debug!(
            category = ?event.category,
            operation = %event.operation,
            direct = is_direct,
            "received event"
        );

        // Subscription filter first: drop broadcast events we don't subscribe to.
        // This runs BEFORE the pause check so unsubscribed events never enter the queue.
        if !is_direct && !self.subscriptions.contains(&event.category) {
            debug!(category = ?event.category, "skipping event (not subscribed)");
            return true;
        }

        // Queue events while paused
        if is_channel_paused(&self.world, self.id).await {
            self.paused_queue
                .try_enqueue(event, "ChannelRunner", self.id);
            return true;
        }

        self.process_event(event, is_direct).await;
        true
    }

    /// Processes a single event by delivering it to the Component.
    ///
    /// For broadcast events (`is_direct == false`), the subscription filter
    /// is applied — events whose category is not subscribed are skipped.
    /// For direct events (`is_direct == true`), the filter is bypassed.
    async fn process_event(&self, event: Event, is_direct: bool) {
        // Subscription filter: skip broadcast events for categories we don't subscribe to
        if !is_direct && !self.subscriptions.contains(&event.category) {
            debug!(category = ?event.category, "skipping event (not subscribed)");
            return;
        }

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
                debug!(response = ?response, "component returned success");
            }
            Err(e) => {
                warn!(error = %e, "component returned error");
            }
        }
    }

    /// Drains the paused queue and processes all queued events.
    ///
    /// Queued events have already passed the subscription filter in
    /// `handle_event()`, so they are processed without re-filtering.
    async fn drain_paused_queue(&mut self) {
        // Collect events first to avoid borrow issues with async process_event
        let events: Vec<_> = self.paused_queue.drain("ChannelRunner", self.id).collect();

        for event in events {
            self.process_event(event, true).await;
        }
    }

    /// Handles an incoming RPC request from another Component.
    ///
    /// Calls `Component::on_request()` and sends the result back through
    /// the envelope's `reply_tx`.
    async fn handle_rpc_request(&self, envelope: RequestEnvelope) {
        debug!(
            request_id = %envelope.request.id,
            operation = %envelope.request.operation,
            source = %envelope.request.source,
            "handling RPC request"
        );

        let result = {
            let mut comp = self.component.lock().await;
            comp.on_request(&envelope.request)
        };

        let response = result.map_err(|e| e.to_string());
        if envelope.reply_tx.send(response).is_err() {
            debug!("RPC reply dropped (caller cancelled)");
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
        let (runner, handle) = ChannelRunner::builder(
            child_id,
            self.world_tx.clone(),
            Arc::clone(&self.world),
            signal_rx,
            component,
        )
        .build();

        Some((runner, handle))
    }
}

/// Builder for creating [`ChannelRunner`] instances.
///
/// This consolidates the various constructor patterns into a fluent API:
///
/// ```ignore
/// // Basic runner (no emitter, no child spawner)
/// let (runner, handle) = ChannelRunnerBuilder::new(id, world_tx, world, signal_rx, component)
///     .build();
///
/// // With emitter only
/// let (runner, handle) = ChannelRunnerBuilder::new(id, world_tx, world, signal_rx, component)
///     .with_emitter(signal_tx)
///     .build();
///
/// // With child spawner only
/// let (runner, handle) = ChannelRunnerBuilder::new(id, world_tx, world, signal_rx, component)
///     .with_child_spawner(signal_tx)
///     .build();
///
/// // With both emitter and child spawner
/// let (runner, handle) = ChannelRunnerBuilder::new(id, world_tx, world, signal_rx, component)
///     .with_emitter(signal_tx.clone())
///     .with_child_spawner(signal_tx)
///     .build();
/// ```
pub struct ChannelRunnerBuilder {
    id: ChannelId,
    world_tx: mpsc::Sender<WorldCommand>,
    world: Arc<RwLock<World>>,
    signal_rx: broadcast::Receiver<Signal>,
    component: Box<dyn Component>,
    /// Signal sender for emitter (if enabled).
    emitter_signal_tx: Option<broadcast::Sender<Signal>>,
    /// Output channel for routing Output events to IO channel.
    output_tx: Option<OutputSender>,
    /// Enable child spawner.
    enable_child_spawner: bool,
    /// Lua child loader for spawning Lua children.
    lua_loader: Option<Arc<dyn LuaChildLoader>>,
    /// Component loader for spawning components as runners.
    component_loader: Option<Arc<dyn ComponentLoader>>,
    /// Session for permission checking (identity + privilege level).
    session: Option<Arc<Session>>,
    /// Permission checker policy.
    checker: Option<Arc<dyn PermissionChecker>>,
    /// Dynamic command grants.
    grants: Option<Arc<dyn orcs_auth::GrantPolicy>>,
    /// Shared channel handles for event broadcasting via Emitter.
    shared_handles: Option<SharedChannelHandles>,
    /// Shared Board for auto-recording emitted events.
    board: Option<crate::board::SharedBoard>,
    /// Initial snapshot to restore before init (session resume).
    initial_snapshot: Option<ComponentSnapshot>,
    /// Enable request channel for Component-to-Component RPC.
    enable_request_channel: bool,
    /// Shared ComponentId → ChannelId mapping for RPC routing via Emitter.
    component_channel_map: Option<crate::engine::SharedComponentChannelMap>,
}

impl ChannelRunnerBuilder {
    /// Creates a new builder with required parameters.
    #[must_use]
    pub fn new(
        id: ChannelId,
        world_tx: mpsc::Sender<WorldCommand>,
        world: Arc<RwLock<World>>,
        signal_rx: broadcast::Receiver<Signal>,
        component: Box<dyn Component>,
    ) -> Self {
        Self {
            id,
            world_tx,
            world,
            signal_rx,
            component,
            emitter_signal_tx: None,
            output_tx: None,
            enable_child_spawner: false,
            lua_loader: None,
            component_loader: None,
            session: None,
            checker: None,
            grants: None,
            shared_handles: None,
            board: None,
            initial_snapshot: None,
            enable_request_channel: false,
            component_channel_map: None,
        }
    }

    /// Enables the EventEmitter for this runner.
    ///
    /// The emitter allows the Component to emit output events via `orcs.output()`.
    #[must_use]
    pub fn with_emitter(mut self, signal_tx: broadcast::Sender<Signal>) -> Self {
        self.emitter_signal_tx = Some(signal_tx);
        self
    }

    /// Sets the output channel for routing Output events.
    ///
    /// When set, the Component's `emit_output()` calls will send events
    /// to this channel instead of the runner's own event channel.
    /// This enables ChannelRunner components to display output via
    /// ClientRunner's IOBridge.
    ///
    /// # Arguments
    ///
    /// * `output_tx` - Sender for the IO channel's event_rx
    #[must_use]
    pub fn with_output_channel(mut self, output_tx: OutputSender) -> Self {
        self.output_tx = Some(output_tx);
        self
    }

    /// Enables the ChildSpawner for this runner.
    ///
    /// This allows the Component and its Children to spawn sub-children
    /// via ChildContext.
    ///
    /// # Arguments
    ///
    /// * `loader` - Optional Lua child loader for spawning Lua children
    #[must_use]
    pub fn with_child_spawner(mut self, loader: Option<Arc<dyn LuaChildLoader>>) -> Self {
        self.enable_child_spawner = true;
        self.lua_loader = loader;
        self
    }

    /// Sets the component loader for spawning components as runners.
    ///
    /// This enables the ChildContext::spawn_runner_from_script() functionality.
    ///
    /// # Arguments
    ///
    /// * `loader` - Component loader for creating components from scripts
    #[must_use]
    pub fn with_component_loader(mut self, loader: Arc<dyn ComponentLoader>) -> Self {
        self.component_loader = Some(loader);
        self
    }

    /// Sets the session for permission checking.
    ///
    /// Takes ownership of a Session and wraps it in Arc.
    /// For sharing a Session across multiple runners, use [`with_session_arc`].
    ///
    /// When set, operations like `orcs.exec()`, `spawn_child()`, and `spawn_runner()`
    /// will be checked against the session's privilege level.
    ///
    /// # Arguments
    ///
    /// * `session` - Session to use for permission checks
    #[must_use]
    pub fn with_session(mut self, session: Session) -> Self {
        self.session = Some(Arc::new(session));
        self
    }

    /// Sets the session for permission checking (Arc version).
    ///
    /// Use this when sharing a Session across multiple runners,
    /// so that dynamic grants (via HIL) are shared.
    ///
    /// # Arguments
    ///
    /// * `session` - Arc-wrapped Session for shared access
    #[must_use]
    pub fn with_session_arc(mut self, session: Arc<Session>) -> Self {
        self.session = Some(session);
        self
    }

    /// Sets the permission checker policy.
    ///
    /// # Arguments
    ///
    /// * `checker` - Permission checker implementation
    #[must_use]
    pub fn with_checker(mut self, checker: Arc<dyn PermissionChecker>) -> Self {
        self.checker = Some(checker);
        self
    }

    /// Sets the dynamic command grant store.
    ///
    /// # Arguments
    ///
    /// * `grants` - Grant policy implementation for dynamic command permissions
    #[must_use]
    pub fn with_grants(mut self, grants: Arc<dyn orcs_auth::GrantPolicy>) -> Self {
        self.grants = Some(grants);
        self
    }

    /// Sets the shared channel handles for event broadcasting.
    ///
    /// When set, the EventEmitter's `emit_event()` will broadcast
    /// Extension events to all registered channels.
    #[must_use]
    pub fn with_shared_handles(mut self, handles: SharedChannelHandles) -> Self {
        self.shared_handles = Some(handles);
        self
    }

    /// Sets the shared component-to-channel mapping for RPC routing.
    ///
    /// When set, the EventEmitter's `request()` can resolve target
    /// ComponentId to ChannelId and route RPC via ChannelHandle.
    #[must_use]
    pub fn with_component_channel_map(
        mut self,
        map: crate::engine::SharedComponentChannelMap,
    ) -> Self {
        self.component_channel_map = Some(map);
        self
    }

    /// Sets the shared Board for auto-recording emitted events.
    ///
    /// When set, the EventEmitter will automatically append entries
    /// to the Board on `emit_output()` and `emit_event()`.
    #[must_use]
    pub fn with_board(mut self, board: crate::board::SharedBoard) -> Self {
        self.board = Some(board);
        self
    }

    /// Sets the initial snapshot to restore before `init()`.
    ///
    /// When set, the Component's `restore()` method is called with
    /// this snapshot before `init()` during `run()`. Used for
    /// session resume.
    ///
    /// # Arguments
    ///
    /// * `snapshot` - Snapshot to restore from
    #[must_use]
    pub fn with_initial_snapshot(mut self, snapshot: ComponentSnapshot) -> Self {
        self.initial_snapshot = Some(snapshot);
        self
    }

    /// Enables the request channel for Component-to-Component RPC.
    ///
    /// When enabled, the runner's [`ChannelHandle`] will accept
    /// [`RequestEnvelope`]s via `send_request()`, allowing other
    /// Components to call this Component's `on_request()` and receive
    /// a response.
    #[must_use]
    pub fn with_request_channel(mut self) -> Self {
        self.enable_request_channel = true;
        self
    }

    /// Configures auth (session/checker/grants) and IO output routing on a [`ChildContextImpl`].
    ///
    /// Extracted from `build()` to eliminate duplication between the child-spawner
    /// branch and the auth-only branch.
    fn configure_context(
        &mut self,
        mut ctx: ChildContextImpl,
        io_output_tx: &Option<OutputSender>,
        component_id: &str,
    ) -> ChildContextImpl {
        if let Some(session) = self.session.take() {
            ctx = ctx.with_session_arc(session);
            info!("ChannelRunnerBuilder: enabled session for {}", component_id);
        }
        if let Some(checker) = self.checker.take() {
            ctx = ctx.with_checker(checker);
            info!(
                "ChannelRunnerBuilder: enabled permission checker for {}",
                component_id
            );
        }
        if let Some(grants) = self.grants.take() {
            ctx = ctx.with_grants(grants);
            info!(
                "ChannelRunnerBuilder: enabled grant store for {}",
                component_id
            );
        }
        if let Some(io_tx) = io_output_tx.clone() {
            ctx = ctx.with_io_output_channel(io_tx);
            info!(
                "ChannelRunnerBuilder: enabled IO output routing for {}",
                component_id
            );
        }
        ctx
    }

    /// Builds the ChannelRunner and returns it with a ChannelHandle.
    #[must_use]
    pub fn build(mut self) -> (ChannelRunner, ChannelHandle) {
        let (event_tx, event_rx) = mpsc::channel(EVENT_BUFFER_SIZE);

        // Clone output_tx for ChildContext IO routing (before emitter takes it)
        let io_output_tx = self.output_tx.as_ref().cloned();

        // Set up emitter if enabled
        if let Some(signal_tx) = &self.emitter_signal_tx {
            let component_id = self.component.id().clone();
            let mut emitter = EventEmitter::new(
                OutputSender::new(event_tx.clone()),
                signal_tx.clone(),
                component_id.clone(),
            );

            // Route Output events to IO channel if configured
            if let Some(output_tx) = self.output_tx.take() {
                emitter = emitter.with_output_channel(output_tx);
                info!(
                    "ChannelRunnerBuilder: routing output to IO channel for {}",
                    component_id.fqn()
                );
            }

            // Enable event broadcasting if shared handles are provided
            if let Some(handles) = self.shared_handles.take() {
                emitter = emitter.with_shared_handles(handles);
                info!(
                    "ChannelRunnerBuilder: enabled event broadcast for {}",
                    component_id.fqn()
                );
            }

            // Enable RPC routing if component channel map is provided
            if let Some(map) = self.component_channel_map.take() {
                emitter = emitter.with_component_channel_map(map, self.id);
            }

            // Attach Board for auto-recording
            if let Some(board) = self.board.take() {
                emitter = emitter.with_board(board);
            }

            self.component.set_emitter(Box::new(emitter));

            info!(
                "ChannelRunnerBuilder: injected emitter for {}",
                component_id.fqn()
            );
        }

        // Set up child spawner if enabled
        let child_spawner = if self.enable_child_spawner {
            let component_id = self.component.id().fqn();
            let output_sender = OutputSender::new(event_tx.clone());
            let spawner = ChildSpawner::new(&component_id, output_sender.clone());
            let spawner_arc = Arc::new(StdMutex::new(spawner));

            // Create ChildContext and inject into Component
            let mut ctx =
                ChildContextImpl::new(&component_id, output_sender, Arc::clone(&spawner_arc));

            // Add Lua loader if provided
            if let Some(loader) = self.lua_loader.take() {
                ctx = ctx.with_lua_loader(loader);
                info!(
                    "ChannelRunnerBuilder: created spawner with Lua loader for {}",
                    component_id
                );
            } else {
                info!(
                    "ChannelRunnerBuilder: created spawner (no Lua loader) for {}",
                    component_id
                );
            }

            // Enable runner spawning if signal emitter is available
            if let Some(signal_tx) = &self.emitter_signal_tx {
                ctx = ctx.with_runner_support(
                    self.world_tx.clone(),
                    Arc::clone(&self.world),
                    signal_tx.clone(),
                );
                info!(
                    "ChannelRunnerBuilder: enabled runner spawning for {}",
                    component_id
                );
            }

            // Add component loader for spawn_runner_from_script
            if let Some(loader) = self.component_loader.take() {
                ctx = ctx.with_component_loader(loader);
                info!(
                    "ChannelRunnerBuilder: enabled component loader for {}",
                    component_id
                );
            }

            // Add session, checker, grants, and IO output routing
            ctx = self.configure_context(ctx, &io_output_tx, &component_id);

            self.component.set_child_context(Box::new(ctx));

            Some(spawner_arc)
        } else if self.session.is_some() || self.checker.is_some() || self.grants.is_some() {
            // No child spawner, but auth context is needed for permission-checked orcs.exec()
            let component_id = self.component.id().fqn();
            let dummy_output = OutputSender::new(event_tx.clone());
            let dummy_spawner = ChildSpawner::new(&component_id, dummy_output.clone());
            let dummy_arc = Arc::new(StdMutex::new(dummy_spawner));
            let mut ctx =
                ChildContextImpl::new(&component_id, dummy_output, Arc::clone(&dummy_arc));

            ctx = self.configure_context(ctx, &io_output_tx, &component_id);

            self.component.set_child_context(Box::new(ctx));
            info!(
                "ChannelRunnerBuilder: auth-only context injected for {}",
                component_id
            );
            None
        } else {
            None
        };

        // Determine if we need to keep event_tx for child context
        let event_tx_for_context = if self.enable_child_spawner || self.emitter_signal_tx.is_some()
        {
            Some(event_tx.clone())
        } else {
            None
        };

        // Cache subscriptions from Component to avoid locking on every event
        let subscriptions = self.component.subscriptions().to_vec();

        // Create request channel if enabled
        let (request_tx, request_rx) = if self.enable_request_channel {
            let (tx, rx) = mpsc::channel(REQUEST_BUFFER_SIZE);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        let runner = ChannelRunner {
            id: self.id,
            event_rx,
            signal_rx: self.signal_rx,
            world_tx: self.world_tx,
            world: self.world,
            component: Arc::new(Mutex::new(self.component)),
            subscriptions,
            paused_queue: PausedEventQueue::new(),
            child_spawner,
            event_tx: event_tx_for_context,
            request_rx,
            initial_snapshot: self.initial_snapshot,
        };

        let mut handle = ChannelHandle::new(self.id, event_tx);
        handle.request_tx = request_tx;

        (runner, handle)
    }
}

impl ChannelRunner {
    /// Creates a new builder for constructing a ChannelRunner.
    ///
    /// This is the recommended way to create runners with optional features.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let (runner, handle) = ChannelRunner::builder(id, world_tx, world, signal_rx, component)
    ///     .with_emitter(signal_tx)
    ///     .with_child_spawner(None)
    ///     .build();
    /// ```
    #[must_use]
    pub fn builder(
        id: ChannelId,
        world_tx: mpsc::Sender<WorldCommand>,
        world: Arc<RwLock<World>>,
        signal_rx: broadcast::Receiver<Signal>,
        component: Box<dyn Component>,
    ) -> ChannelRunnerBuilder {
        ChannelRunnerBuilder::new(id, world_tx, world, signal_rx, component)
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

        fn subscriptions(&self) -> &[EventCategory] {
            &[EventCategory::Echo, EventCategory::Lifecycle]
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
        let (runner, handle) = ChannelRunner::builder(
            primary,
            world_tx.clone(),
            world,
            signal_rx,
            mock_component(),
        )
        .build();

        assert_eq!(runner.id(), primary);
        assert_eq!(handle.id, primary);

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn runner_receives_events() {
        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let signal_rx = signal_tx.subscribe();
        let (runner, handle) = ChannelRunner::builder(
            primary,
            world_tx.clone(),
            world,
            signal_rx,
            mock_component(),
        )
        .build();

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
        let (runner, _handle) = ChannelRunner::builder(
            primary,
            world_tx.clone(),
            world.clone(),
            signal_rx,
            mock_component(),
        )
        .build();

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
        let (_runner, handle) = ChannelRunner::builder(
            primary,
            world_tx.clone(),
            world,
            signal_rx,
            mock_component(),
        )
        .build();

        let handle2 = handle.clone();
        assert_eq!(handle.id, handle2.id);

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn runner_with_emitter_creation() {
        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let signal_rx = signal_tx.subscribe();
        let (runner, handle) = ChannelRunner::builder(
            primary,
            world_tx.clone(),
            world,
            signal_rx,
            mock_component(),
        )
        .with_emitter(signal_tx.clone())
        .build();

        assert_eq!(runner.id(), primary);
        assert_eq!(handle.id, primary);

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn runner_with_emitter_receives_emitted_events() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc as StdArc;

        // Component that uses emitter to emit output
        struct EmittingComponent {
            id: ComponentId,
            emitter: Option<Box<dyn orcs_component::Emitter>>,
            call_count: StdArc<AtomicUsize>,
        }

        impl EmittingComponent {
            fn new(call_count: StdArc<AtomicUsize>) -> Self {
                Self {
                    id: ComponentId::builtin("emitting"),
                    emitter: None,
                    call_count,
                }
            }
        }

        impl Component for EmittingComponent {
            fn id(&self) -> &ComponentId {
                &self.id
            }

            fn status(&self) -> Status {
                Status::Idle
            }

            fn subscriptions(&self) -> &[EventCategory] {
                &[EventCategory::Echo]
            }

            fn on_request(&mut self, request: &Request) -> Result<Value, ComponentError> {
                self.call_count.fetch_add(1, Ordering::SeqCst);
                // Emit output via emitter when receiving a request
                if let Some(emitter) = &self.emitter {
                    emitter.emit_output("Response from component");
                }
                Ok(request.payload.clone())
            }

            fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
                if signal.is_veto() {
                    SignalResponse::Abort
                } else {
                    SignalResponse::Handled
                }
            }

            fn abort(&mut self) {}

            fn set_emitter(&mut self, emitter: Box<dyn orcs_component::Emitter>) {
                self.emitter = Some(emitter);
            }
        }

        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let call_count = StdArc::new(AtomicUsize::new(0));
        let component = Box::new(EmittingComponent::new(StdArc::clone(&call_count)));

        let signal_rx = signal_tx.subscribe();
        let (runner, handle) =
            ChannelRunner::builder(primary, world_tx.clone(), world, signal_rx, component)
                .with_emitter(signal_tx.clone())
                .build();

        let runner_task = tokio::spawn(runner.run());

        // Inject an event
        let event = Event {
            category: EventCategory::Echo,
            operation: "test".to_string(),
            source: ComponentId::builtin("test"),
            payload: serde_json::json!({"trigger": true}),
        };
        handle.inject(event).await.unwrap();

        // Wait for processing
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Component should have been called
        assert!(
            call_count.load(Ordering::SeqCst) >= 1,
            "Component should have received the event"
        );

        // Stop runner
        signal_tx
            .send(Signal::cancel(primary, Principal::System))
            .unwrap();

        let _ = tokio::time::timeout(std::time::Duration::from_millis(100), runner_task).await;

        teardown(manager_task, world_tx).await;
    }

    // Builder pattern tests

    #[tokio::test]
    async fn builder_basic() {
        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let signal_rx = signal_tx.subscribe();
        let (runner, handle) = ChannelRunner::builder(
            primary,
            world_tx.clone(),
            world,
            signal_rx,
            mock_component(),
        )
        .build();

        assert_eq!(runner.id(), primary);
        assert_eq!(handle.id, primary);
        assert!(runner.child_spawner().is_none());

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn builder_with_emitter() {
        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let signal_rx = signal_tx.subscribe();
        let (runner, handle) = ChannelRunner::builder(
            primary,
            world_tx.clone(),
            world,
            signal_rx,
            mock_component(),
        )
        .with_emitter(signal_tx.clone())
        .build();

        assert_eq!(runner.id(), primary);
        assert_eq!(handle.id, primary);

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn builder_with_child_spawner() {
        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let signal_rx = signal_tx.subscribe();
        let (runner, handle) = ChannelRunner::builder(
            primary,
            world_tx.clone(),
            world,
            signal_rx,
            mock_component(),
        )
        .with_child_spawner(None)
        .build();

        assert_eq!(runner.id(), primary);
        assert_eq!(handle.id, primary);
        assert!(runner.child_spawner().is_some());

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn builder_with_full_support() {
        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let signal_rx = signal_tx.subscribe();
        let (runner, handle) = ChannelRunner::builder(
            primary,
            world_tx.clone(),
            world,
            signal_rx,
            mock_component(),
        )
        .with_emitter(signal_tx.clone())
        .with_child_spawner(None)
        .build();

        assert_eq!(runner.id(), primary);
        assert_eq!(handle.id, primary);
        assert!(runner.child_spawner().is_some());

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn builder_creates_child_context() {
        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let signal_rx = signal_tx.subscribe();
        let (runner, _handle) = ChannelRunner::builder(
            primary,
            world_tx.clone(),
            world,
            signal_rx,
            mock_component(),
        )
        .with_child_spawner(None)
        .build();

        // Should be able to create child context when spawner is enabled
        let ctx = runner.create_child_context("child-1");
        assert!(ctx.is_some());

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn builder_no_child_context_without_spawner() {
        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let signal_rx = signal_tx.subscribe();
        let (runner, _handle) = ChannelRunner::builder(
            primary,
            world_tx.clone(),
            world,
            signal_rx,
            mock_component(),
        )
        .build();

        // Should NOT be able to create child context when spawner is disabled
        let ctx = runner.create_child_context("child-1");
        assert!(ctx.is_none());

        teardown(manager_task, world_tx).await;
    }

    // === Request Channel (Component-to-Component RPC) Tests ===

    #[tokio::test]
    async fn builder_with_request_channel_enables_accepts_requests() {
        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let signal_rx = signal_tx.subscribe();
        let (_runner, handle) = ChannelRunner::builder(
            primary,
            world_tx.clone(),
            world,
            signal_rx,
            mock_component(),
        )
        .with_request_channel()
        .build();

        assert!(handle.accepts_requests());

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn builder_without_request_channel_rejects_requests() {
        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let signal_rx = signal_tx.subscribe();
        let (_runner, handle) = ChannelRunner::builder(
            primary,
            world_tx.clone(),
            world,
            signal_rx,
            mock_component(),
        )
        .build();

        assert!(!handle.accepts_requests());

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn rpc_request_routed_to_runner_and_responded() {
        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let signal_rx = signal_tx.subscribe();
        let (runner, handle) = ChannelRunner::builder(
            primary,
            world_tx.clone(),
            world,
            signal_rx,
            mock_component(),
        )
        .with_request_channel()
        .build();

        let runner_task = tokio::spawn(runner.run());

        // Send RPC request via handle
        let source = ComponentId::builtin("caller");
        let target = ComponentId::builtin("test");
        let req = Request::new(
            EventCategory::Echo,
            "echo",
            source,
            primary,
            Value::String("rpc_payload".into()),
        )
        .with_target(target);

        let (reply_tx, reply_rx) = oneshot::channel();
        handle.send_request(req, reply_tx).await.unwrap();

        // Should receive the echoed payload back
        let result = tokio::time::timeout(std::time::Duration::from_millis(200), reply_rx)
            .await
            .expect("reply should arrive within timeout")
            .expect("reply channel should not be dropped");

        assert_eq!(result.unwrap(), Value::String("rpc_payload".into()));

        // Cleanup
        signal_tx
            .send(Signal::cancel(primary, Principal::System))
            .unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_millis(100), runner_task).await;
        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn rpc_request_component_error_returned_as_string() {
        /// Component that always returns error from on_request.
        struct FailingComponent {
            id: ComponentId,
        }

        impl Component for FailingComponent {
            fn id(&self) -> &ComponentId {
                &self.id
            }
            fn status(&self) -> Status {
                Status::Idle
            }
            fn on_request(&mut self, _request: &Request) -> Result<Value, ComponentError> {
                Err(ComponentError::ExecutionFailed(
                    "deliberate test failure".into(),
                ))
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

        let (manager_task, world_tx, world, signal_tx, primary) = setup().await;

        let component: Box<dyn Component> = Box::new(FailingComponent {
            id: ComponentId::builtin("failing"),
        });
        let signal_rx = signal_tx.subscribe();
        let (runner, handle) =
            ChannelRunner::builder(primary, world_tx.clone(), world, signal_rx, component)
                .with_request_channel()
                .build();

        let runner_task = tokio::spawn(runner.run());

        let source = ComponentId::builtin("caller");
        let target = ComponentId::builtin("failing");
        let req = Request::new(EventCategory::Echo, "op", source, primary, Value::Null)
            .with_target(target);

        let (reply_tx, reply_rx) = oneshot::channel();
        handle.send_request(req, reply_tx).await.unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_millis(200), reply_rx)
            .await
            .expect("reply should arrive")
            .expect("channel should not be dropped");

        assert!(result.is_err(), "should return Err for component failure");
        assert!(
            result.unwrap_err().contains("deliberate test failure"),
            "error message should contain original error"
        );

        signal_tx
            .send(Signal::cancel(primary, Principal::System))
            .unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_millis(100), runner_task).await;
        teardown(manager_task, world_tx).await;
    }
}

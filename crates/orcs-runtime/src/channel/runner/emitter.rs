//! Event emitter for Components to emit events.
//!
//! Components can emit events to:
//! - Their owning Channel (via `emit`) - processed by ClientRunner
//! - All Components (via `broadcast`) - Signal broadcast
//!
//! # Usage
//!
//! ```ignore
//! // Component receives emitter via set_emitter()
//! fn set_emitter(&mut self, emitter: Box<dyn Emitter>) {
//!     self.emitter = Some(emitter);
//! }
//!
//! // Later, emit an Output event
//! if let Some(emitter) = &self.emitter {
//!     emitter.emit_output("Task completed successfully");
//! }
//! ```

use super::base::OutputSender;
use super::Event;
use crate::board::{BoardEntry, BoardEntryKind, SharedBoard};
use crate::engine::{SharedChannelHandles, SharedComponentChannelMap};
use orcs_component::Emitter;
use orcs_event::{EventCategory, Signal};
use orcs_types::{ChannelId, ComponentId};
use serde_json::Value;
use tokio::sync::broadcast;

/// Event emitter for Components.
///
/// Allows Components to emit events to their owning Channel
/// or broadcast signals to all Components.
///
/// Output events can be routed to a separate IO channel when configured,
/// enabling ChannelRunner components to display output via ClientRunner.
#[derive(Clone)]
pub struct EventEmitter {
    /// Sender to emit events to the owning Channel.
    /// All events are sent as [`InboundEvent::Direct`] since these are
    /// internal emissions, not broadcasts.
    channel_tx: OutputSender,
    /// Sender for Output events to IO channel.
    /// If set, emit_output sends here instead of channel_tx.
    output_tx: Option<OutputSender>,
    /// Sender to broadcast signals to all Components.
    signal_tx: broadcast::Sender<Signal>,
    /// Component ID for event source.
    source_id: ComponentId,
    /// Shared channel handles for broadcasting events to all channels.
    shared_handles: Option<SharedChannelHandles>,
    /// Shared ComponentId → ChannelId mapping for RPC routing.
    component_channel_map: Option<SharedComponentChannelMap>,
    /// Channel ID of this emitter's owning ChannelRunner.
    channel_id: Option<ChannelId>,
    /// Shared Board for auto-recording emitted events.
    board: Option<SharedBoard>,
}

impl EventEmitter {
    /// Creates a new EventEmitter.
    ///
    /// # Arguments
    ///
    /// * `channel_tx` - Sender for the owning Channel's event_rx
    /// * `signal_tx` - Broadcast sender for signals
    /// * `source_id` - Component ID to use as event source
    #[must_use]
    pub(crate) fn new(
        channel_tx: OutputSender,
        signal_tx: broadcast::Sender<Signal>,
        source_id: ComponentId,
    ) -> Self {
        Self {
            channel_tx,
            output_tx: None,
            signal_tx,
            source_id,
            shared_handles: None,
            component_channel_map: None,
            channel_id: None,
            board: None,
        }
    }

    /// Sets the output channel for routing Output events to IO channel.
    ///
    /// When set, `emit_output()` will send to this channel instead of
    /// the owning channel. This enables ChannelRunner components to
    /// display output via ClientRunner's IOBridge.
    ///
    /// # Arguments
    ///
    /// * `output_tx` - Sender for the IO channel's event_rx
    #[must_use]
    pub(crate) fn with_output_channel(mut self, output_tx: OutputSender) -> Self {
        self.output_tx = Some(output_tx);
        self
    }

    /// Sets the shared channel handles for event broadcasting.
    ///
    /// When set, `emit_event()` will broadcast Extension events to all
    /// registered channels via these handles.
    #[must_use]
    pub(crate) fn with_shared_handles(mut self, handles: SharedChannelHandles) -> Self {
        self.shared_handles = Some(handles);
        self
    }

    /// Sets the shared component-to-channel mapping for RPC routing.
    ///
    /// When set, `request()` can resolve target ComponentId to ChannelId
    /// and send RPC requests via ChannelHandle.
    #[must_use]
    pub(crate) fn with_component_channel_map(
        mut self,
        map: SharedComponentChannelMap,
        channel_id: ChannelId,
    ) -> Self {
        self.component_channel_map = Some(map);
        self.channel_id = Some(channel_id);
        self
    }

    /// Sets the shared Board for auto-recording emitted events.
    ///
    /// When set, `emit_output()` and `emit_event()` will automatically
    /// append entries to the Board for cross-component visibility.
    #[must_use]
    pub(crate) fn with_board(mut self, board: SharedBoard) -> Self {
        self.board = Some(board);
        self
    }

    /// Appends an entry to the Board (if attached).
    fn record_to_board(&self, kind: BoardEntryKind, operation: &str, payload: &serde_json::Value) {
        if let Some(board) = &self.board {
            let entry = BoardEntry {
                timestamp: chrono::Utc::now(),
                source: self.source_id.clone(),
                kind,
                operation: operation.to_string(),
                payload: payload.clone(),
            };
            board.write().append(entry);
        }
    }

    /// Emits an event to the owning Channel.
    ///
    /// The ClientRunner will receive this event and process it.
    /// For `Output` category events, ClientRunner will send to IOBridge.
    ///
    /// # Arguments
    ///
    /// * `event` - The event to emit
    ///
    /// # Returns
    ///
    /// `true` if the event was sent successfully, `false` if the channel is full or closed.
    pub fn emit(&self, event: Event) -> bool {
        self.channel_tx.try_send_direct(event).is_ok()
    }

    /// Emits an Output event with a message.
    ///
    /// If an output channel is configured (via `with_output_channel`),
    /// the event is sent there. Otherwise, it's sent to the owning channel.
    /// ClientRunner will send this to IOBridge for display.
    ///
    /// # Arguments
    ///
    /// * `message` - The message to display
    pub fn emit_output(&self, message: &str) {
        let payload = serde_json::json!({
            "message": message,
            "level": "info"
        });
        self.record_to_board(
            BoardEntryKind::Output {
                level: "info".to_string(),
            },
            "display",
            &payload,
        );
        let event = Event {
            category: EventCategory::Output,
            operation: "display".to_string(),
            source: self.source_id.clone(),
            payload,
        };
        let ok = self.emit_to_output(event);
        tracing::debug!(source = %self.source_id.fqn(), success = ok, "emit_output");
    }

    /// Emits an Output event with a specific level.
    ///
    /// If an output channel is configured, sends there; otherwise to owning channel.
    ///
    /// # Arguments
    ///
    /// * `message` - The message to display
    /// * `level` - Log level ("info", "warn", "error")
    pub fn emit_output_with_level(&self, message: &str, level: &str) {
        let payload = serde_json::json!({
            "message": message,
            "level": level
        });
        self.record_to_board(
            BoardEntryKind::Output {
                level: level.to_string(),
            },
            "display",
            &payload,
        );
        let event = Event {
            category: EventCategory::Output,
            operation: "display".to_string(),
            source: self.source_id.clone(),
            payload,
        };
        let _ = self.emit_to_output(event);
    }

    /// Emits an event to the output channel (or owning channel if not configured).
    ///
    /// This is the internal method used by emit_output variants.
    fn emit_to_output(&self, event: Event) -> bool {
        if let Some(output_tx) = &self.output_tx {
            match output_tx.try_send_direct(event) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(
                        source = %self.source_id.fqn(),
                        "emit_to_output: output_tx send failed (channel full or closed): {}",
                        e
                    );
                    false
                }
            }
        } else {
            tracing::warn!(
                source = %self.source_id.fqn(),
                "emit_to_output: output_tx is None, falling back to own channel"
            );
            self.emit(event)
        }
    }

    /// Broadcasts a signal to all Components.
    ///
    /// Use this when the Component needs to notify all other
    /// Components of something (e.g., state change).
    ///
    /// # Arguments
    ///
    /// * `signal` - The signal to broadcast
    ///
    /// # Returns
    ///
    /// `true` if the signal was sent successfully.
    pub fn broadcast(&self, signal: Signal) -> bool {
        let ok = self.signal_tx.send(signal).is_ok();
        tracing::debug!(source = %self.source_id.fqn(), success = ok, "broadcast signal");
        ok
    }

    /// Broadcasts a custom Extension event to all registered channels.
    ///
    /// Creates an `Extension { namespace: "lua", kind: category }` event
    /// and broadcasts it to all channels via shared handles. Channels
    /// subscribed to the matching Extension category will process it.
    ///
    /// Falls back to emitting to own channel if shared handles are not set.
    ///
    /// # Arguments
    ///
    /// * `category` - Extension kind string (e.g., "tool:result")
    /// * `operation` - Operation name (e.g., "complete")
    /// * `payload` - Event payload data
    ///
    /// # Returns
    ///
    /// `true` if at least one channel received the event.
    pub fn emit_event(&self, category: &str, operation: &str, payload: serde_json::Value) -> bool {
        self.record_to_board(
            BoardEntryKind::Event {
                category: category.to_string(),
            },
            operation,
            &payload,
        );
        let event = Event {
            category: EventCategory::Extension {
                namespace: "lua".to_string(),
                kind: category.to_string(),
            },
            operation: operation.to_string(),
            source: self.source_id.clone(),
            payload,
        };

        if let Some(handles) = &self.shared_handles {
            let handles = handles.read();
            let mut delivered = 0usize;
            for handle in handles.values() {
                if handle.try_inject(event.clone()).is_ok() {
                    delivered += 1;
                }
            }
            tracing::debug!(
                source = %self.source_id.fqn(),
                category = category,
                operation = operation,
                delivered = delivered,
                "emit_event broadcast"
            );
            delivered > 0
        } else {
            // Fallback: emit to own channel only
            let ok = self.emit(event);
            tracing::debug!(
                source = %self.source_id.fqn(),
                category = category,
                success = ok,
                "emit_event fallback"
            );
            ok
        }
    }

    /// Returns the source Component ID.
    #[must_use]
    pub fn source_id(&self) -> &ComponentId {
        &self.source_id
    }
}

impl std::fmt::Debug for EventEmitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventEmitter")
            .field("source_id", &self.source_id)
            .finish_non_exhaustive()
    }
}

// Implement Emitter trait from orcs-component
impl Emitter for EventEmitter {
    fn emit_output(&self, message: &str) {
        EventEmitter::emit_output(self, message);
    }

    fn emit_output_with_level(&self, message: &str, level: &str) {
        EventEmitter::emit_output_with_level(self, message, level);
    }

    fn emit_event(&self, category: &str, operation: &str, payload: serde_json::Value) -> bool {
        EventEmitter::emit_event(self, category, operation, payload)
    }

    fn board_recent(&self, n: usize) -> Vec<serde_json::Value> {
        match self.board.as_ref() {
            Some(board) => board.read().recent_as_json(n),
            None => Vec::new(),
        }
    }

    fn request(
        &self,
        target: &str,
        operation: &str,
        payload: Value,
        timeout_ms: Option<u64>,
    ) -> Result<Value, String> {
        let map = self
            .component_channel_map
            .as_ref()
            .ok_or("component_channel_map not configured")?;
        let handles = self
            .shared_handles
            .as_ref()
            .ok_or("shared_handles not configured")?;

        let timeout = timeout_ms.unwrap_or(orcs_event::DEFAULT_TIMEOUT_MS);
        let source_channel = self.channel_id.unwrap_or_else(ChannelId::new);

        // block_in_place + block_on: safe from within a tokio multi-threaded runtime
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(super::rpc::resolve_and_send_rpc(
                super::rpc::RpcParams {
                    component_channel_map: map,
                    shared_handles: handles,
                    target_fqn: target,
                    operation,
                    source_id: self.source_id.clone(),
                    source_channel,
                    payload,
                    timeout_ms: timeout,
                },
            ))
        })
    }

    fn clone_box(&self) -> Box<dyn Emitter> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::runner::base::OutputReceiver;
    use orcs_types::{Principal, SignalScope};

    fn setup() -> (EventEmitter, OutputReceiver, broadcast::Receiver<Signal>) {
        let (channel_tx, channel_rx) = OutputSender::channel(64);
        let (signal_tx, signal_rx) = broadcast::channel(64);
        let source_id = ComponentId::builtin("test");

        let emitter = EventEmitter::new(channel_tx, signal_tx, source_id);
        (emitter, channel_rx, signal_rx)
    }

    #[test]
    fn emit_event() {
        let (emitter, mut channel_rx, _signal_rx) = setup();

        let event = Event {
            category: EventCategory::Echo,
            operation: "test".to_string(),
            source: ComponentId::builtin("test"),
            payload: serde_json::json!({"data": "value"}),
        };

        assert!(emitter.emit(event));

        let received = channel_rx.try_recv().expect("should receive emitted event");
        assert_eq!(received.category, EventCategory::Echo);
        assert_eq!(received.operation, "test");
    }

    #[test]
    fn emit_output() {
        let (emitter, mut channel_rx, _signal_rx) = setup();

        emitter.emit_output("Hello, World!");

        let received = channel_rx
            .try_recv()
            .expect("should receive emit_output event");
        assert_eq!(received.category, EventCategory::Output);
        assert_eq!(received.operation, "display");
        assert_eq!(received.payload["message"], "Hello, World!");
        assert_eq!(received.payload["level"], "info");
    }

    #[test]
    fn emit_output_with_level() {
        let (emitter, mut channel_rx, _signal_rx) = setup();

        emitter.emit_output_with_level("Warning message", "warn");

        let received = channel_rx
            .try_recv()
            .expect("should receive emit_output_with_level event");
        assert_eq!(received.category, EventCategory::Output);
        assert_eq!(received.payload["message"], "Warning message");
        assert_eq!(received.payload["level"], "warn");
    }

    #[tokio::test]
    async fn broadcast_signal() {
        let (emitter, _channel_rx, mut signal_rx) = setup();

        let signal = Signal::new(
            orcs_event::SignalKind::Pause,
            SignalScope::Global,
            Principal::System,
        );

        assert!(emitter.broadcast(signal));

        let received = signal_rx
            .recv()
            .await
            .expect("should receive broadcast signal");
        assert!(matches!(received.kind, orcs_event::SignalKind::Pause));
    }

    #[test]
    fn source_id() {
        let (emitter, _channel_rx, _signal_rx) = setup();
        assert_eq!(emitter.source_id().name, "test");
    }

    #[test]
    fn clone_emitter() {
        let (emitter, mut channel_rx, _signal_rx) = setup();

        let cloned = emitter.clone();
        cloned.emit_output("From clone");

        let received = channel_rx
            .try_recv()
            .expect("should receive event from cloned emitter");
        assert_eq!(received.payload["message"], "From clone");
    }

    #[test]
    fn emit_event_without_shared_handles_falls_back_to_own_channel() {
        let (emitter, mut channel_rx, _signal_rx) = setup();

        let result = emitter.emit_event(
            "tool:result",
            "complete",
            serde_json::json!({"tool": "read", "success": true}),
        );
        assert!(result);

        let received = channel_rx
            .try_recv()
            .expect("should receive Extension event via fallback");
        assert_eq!(
            received.category,
            EventCategory::Extension {
                namespace: "lua".to_string(),
                kind: "tool:result".to_string(),
            }
        );
        assert_eq!(received.operation, "complete");
        assert_eq!(received.payload["tool"], "read");
        assert_eq!(received.payload["success"], true);
    }

    #[test]
    fn emit_event_with_shared_handles_broadcasts() {
        use crate::channel::runner::base::ChannelHandle;
        use orcs_types::ChannelId;
        use parking_lot::RwLock;
        use std::collections::HashMap;
        use std::sync::Arc;

        let (channel_tx, _channel_rx) = OutputSender::channel(64);
        let (signal_tx, _signal_rx) = broadcast::channel(64);
        let source_id = ComponentId::builtin("test");

        // Create two target channels
        let ch1 = ChannelId::new();
        let ch2 = ChannelId::new();
        let (tx1, mut rx1) = tokio::sync::mpsc::channel(32);
        let (tx2, mut rx2) = tokio::sync::mpsc::channel(32);

        let mut handles = HashMap::new();
        handles.insert(ch1, ChannelHandle::new(ch1, tx1));
        handles.insert(ch2, ChannelHandle::new(ch2, tx2));
        let shared = Arc::new(RwLock::new(handles));

        let emitter =
            EventEmitter::new(channel_tx, signal_tx, source_id).with_shared_handles(shared);

        let result = emitter.emit_event(
            "tool:result",
            "complete",
            serde_json::json!({"data": "test"}),
        );
        assert!(result);

        // Both channels should receive the event as Broadcast
        let evt1 = rx1
            .try_recv()
            .expect("channel 1 should receive broadcast event");
        let evt2 = rx2
            .try_recv()
            .expect("channel 2 should receive broadcast event");

        assert!(!evt1.is_direct()); // Should be Broadcast
        assert!(!evt2.is_direct());

        let e1 = evt1.into_event();
        let e2 = evt2.into_event();

        assert_eq!(
            e1.category,
            EventCategory::Extension {
                namespace: "lua".to_string(),
                kind: "tool:result".to_string(),
            }
        );
        assert_eq!(e1.operation, "complete");
        assert_eq!(e2.payload["data"], "test");
    }

    #[test]
    fn emit_event_via_trait() {
        let (emitter, mut channel_rx, _signal_rx) = setup();

        let boxed: Box<dyn Emitter> = Box::new(emitter);
        let result = boxed.emit_event(
            "custom:event",
            "notify",
            serde_json::json!({"key": "value"}),
        );
        assert!(result);

        let received = channel_rx
            .try_recv()
            .expect("should receive Extension event via trait");
        assert_eq!(
            received.category,
            EventCategory::Extension {
                namespace: "lua".to_string(),
                kind: "custom:event".to_string(),
            }
        );
    }

    // === Board integration tests ===

    fn setup_with_board() -> (EventEmitter, OutputReceiver, crate::board::SharedBoard) {
        let (channel_tx, channel_rx) = OutputSender::channel(64);
        let (signal_tx, _signal_rx) = broadcast::channel(64);
        let source_id = ComponentId::builtin("test");
        let board = crate::board::shared_board();

        let emitter = EventEmitter::new(channel_tx, signal_tx, source_id).with_board(board.clone());
        (emitter, channel_rx, board)
    }

    #[test]
    fn emit_output_records_to_board() {
        let (emitter, _channel_rx, board) = setup_with_board();

        emitter.emit_output("hello board");

        let b = board.read();
        assert_eq!(b.len(), 1);
        let entries = b.recent(1);
        assert_eq!(entries[0].payload["message"], "hello board");
        assert_eq!(
            entries[0].kind,
            crate::board::BoardEntryKind::Output {
                level: "info".into()
            }
        );
    }

    #[test]
    fn emit_output_with_level_records_to_board() {
        let (emitter, _channel_rx, board) = setup_with_board();

        emitter.emit_output_with_level("warning!", "warn");

        let b = board.read();
        assert_eq!(b.len(), 1);
        let entries = b.recent(1);
        assert_eq!(entries[0].payload["level"], "warn");
        assert_eq!(
            entries[0].kind,
            crate::board::BoardEntryKind::Output {
                level: "warn".into()
            }
        );
    }

    #[test]
    fn emit_event_records_to_board() {
        let (emitter, _channel_rx, board) = setup_with_board();

        emitter.emit_event(
            "tool:result",
            "complete",
            serde_json::json!({"tool": "read"}),
        );

        let b = board.read();
        assert_eq!(b.len(), 1);
        let entries = b.recent(1);
        assert_eq!(entries[0].operation, "complete");
        assert_eq!(
            entries[0].kind,
            crate::board::BoardEntryKind::Event {
                category: "tool:result".into()
            }
        );
    }

    #[test]
    fn board_recent_via_trait() {
        let (emitter, _channel_rx, _board) = setup_with_board();

        emitter.emit_output("msg1");
        emitter.emit_output("msg2");
        emitter.emit_output("msg3");

        let boxed: Box<dyn Emitter> = Box::new(emitter);
        let recent = boxed.board_recent(2);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0]["payload"]["message"], "msg2");
        assert_eq!(recent[1]["payload"]["message"], "msg3");
    }

    #[test]
    fn board_recent_without_board_returns_empty() {
        let (emitter, _channel_rx, _signal_rx) = setup();

        emitter.emit_output("no board");
        let boxed: Box<dyn Emitter> = Box::new(emitter);
        let recent = boxed.board_recent(10);
        assert!(recent.is_empty());
    }

    // === output_channel routing tests ===

    fn setup_with_output_channel() -> (
        EventEmitter,
        OutputReceiver,
        OutputReceiver,
        broadcast::Receiver<Signal>,
    ) {
        let (channel_tx, channel_rx) = OutputSender::channel(64);
        let (output_tx, output_rx) = OutputSender::channel(64);
        let (signal_tx, signal_rx) = broadcast::channel(64);
        let source_id = ComponentId::builtin("test-output-routing");

        let emitter =
            EventEmitter::new(channel_tx, signal_tx, source_id).with_output_channel(output_tx);
        (emitter, channel_rx, output_rx, signal_rx)
    }

    #[test]
    fn emit_output_routes_to_output_channel() {
        let (emitter, mut channel_rx, mut output_rx, _signal_rx) = setup_with_output_channel();

        emitter.emit_output("Hello via output channel!");

        // Output event should arrive on output_rx (IO channel)
        let received = output_rx
            .try_recv()
            .expect("Output event should arrive on output channel");
        assert_eq!(received.category, EventCategory::Output);
        assert_eq!(received.operation, "display");
        assert_eq!(received.payload["message"], "Hello via output channel!");
        assert_eq!(received.payload["level"], "info");

        // Own channel should NOT receive the Output event
        assert!(
            channel_rx.try_recv().is_err(),
            "Own channel should NOT receive Output event when output_channel is configured"
        );
    }

    #[test]
    fn emit_output_with_level_routes_to_output_channel() {
        let (emitter, mut channel_rx, mut output_rx, _signal_rx) = setup_with_output_channel();

        emitter.emit_output_with_level("Warning!", "warn");

        let received = output_rx
            .try_recv()
            .expect("Output event should arrive on output channel");
        assert_eq!(received.category, EventCategory::Output);
        assert_eq!(received.payload["message"], "Warning!");
        assert_eq!(received.payload["level"], "warn");

        assert!(
            channel_rx.try_recv().is_err(),
            "Own channel should NOT receive Output event when output_channel is configured"
        );
    }

    #[test]
    fn emit_output_with_output_channel_also_records_to_board() {
        let (channel_tx, _channel_rx) = OutputSender::channel(64);
        let (output_tx, mut output_rx) = OutputSender::channel(64);
        let (signal_tx, _signal_rx) = broadcast::channel(64);
        let source_id = ComponentId::builtin("test-board-routing");
        let board = crate::board::shared_board();

        let emitter = EventEmitter::new(channel_tx, signal_tx, source_id)
            .with_output_channel(output_tx)
            .with_board(board.clone());

        emitter.emit_output("Board + IO test");

        // Should arrive on output_rx
        let received = output_rx
            .try_recv()
            .expect("Output event should arrive on output channel");
        assert_eq!(received.payload["message"], "Board + IO test");

        // Should also be recorded on Board
        let b = board.read();
        assert_eq!(b.len(), 1, "Board should have exactly 1 entry");
        let entries = b.recent(1);
        assert_eq!(entries[0].payload["message"], "Board + IO test");
    }
}

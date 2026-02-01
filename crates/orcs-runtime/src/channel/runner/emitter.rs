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

use super::Event;
use orcs_component::Emitter;
use orcs_event::{EventCategory, Signal};
use orcs_types::ComponentId;
use tokio::sync::{broadcast, mpsc};

/// Event emitter for Components.
///
/// Allows Components to emit events to their owning Channel
/// or broadcast signals to all Components.
#[derive(Clone)]
pub struct EventEmitter {
    /// Sender to emit events to the owning Channel.
    /// ClientRunner receives these via event_rx.
    channel_tx: mpsc::Sender<Event>,
    /// Sender to broadcast signals to all Components.
    signal_tx: broadcast::Sender<Signal>,
    /// Component ID for event source.
    source_id: ComponentId,
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
    pub fn new(
        channel_tx: mpsc::Sender<Event>,
        signal_tx: broadcast::Sender<Signal>,
        source_id: ComponentId,
    ) -> Self {
        Self {
            channel_tx,
            signal_tx,
            source_id,
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
        self.channel_tx.try_send(event).is_ok()
    }

    /// Emits an Output event with a message.
    ///
    /// Convenience method for emitting display output.
    /// ClientRunner will send this to IOBridge.
    ///
    /// # Arguments
    ///
    /// * `message` - The message to display
    pub fn emit_output(&self, message: &str) {
        let event = Event {
            category: EventCategory::Output,
            operation: "display".to_string(),
            source: self.source_id.clone(),
            payload: serde_json::json!({
                "message": message,
                "level": "info"
            }),
        };
        let _ = self.emit(event);
    }

    /// Emits an Output event with a specific level.
    ///
    /// # Arguments
    ///
    /// * `message` - The message to display
    /// * `level` - Log level ("info", "warn", "error")
    pub fn emit_output_with_level(&self, message: &str, level: &str) {
        let event = Event {
            category: EventCategory::Output,
            operation: "display".to_string(),
            source: self.source_id.clone(),
            payload: serde_json::json!({
                "message": message,
                "level": level
            }),
        };
        let _ = self.emit(event);
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
        self.signal_tx.send(signal).is_ok()
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

    fn clone_box(&self) -> Box<dyn Emitter> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_types::{Principal, SignalScope};

    fn setup() -> (
        EventEmitter,
        mpsc::Receiver<Event>,
        broadcast::Receiver<Signal>,
    ) {
        let (channel_tx, channel_rx) = mpsc::channel(64);
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

        let received = channel_rx.try_recv().unwrap();
        assert_eq!(received.category, EventCategory::Echo);
        assert_eq!(received.operation, "test");
    }

    #[test]
    fn emit_output() {
        let (emitter, mut channel_rx, _signal_rx) = setup();

        emitter.emit_output("Hello, World!");

        let received = channel_rx.try_recv().unwrap();
        assert_eq!(received.category, EventCategory::Output);
        assert_eq!(received.operation, "display");
        assert_eq!(received.payload["message"], "Hello, World!");
        assert_eq!(received.payload["level"], "info");
    }

    #[test]
    fn emit_output_with_level() {
        let (emitter, mut channel_rx, _signal_rx) = setup();

        emitter.emit_output_with_level("Warning message", "warn");

        let received = channel_rx.try_recv().unwrap();
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

        let received = signal_rx.recv().await.unwrap();
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

        let received = channel_rx.try_recv().unwrap();
        assert_eq!(received.payload["message"], "From clone");
    }
}

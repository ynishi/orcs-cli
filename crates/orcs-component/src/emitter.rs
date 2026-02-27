//! Event emitter trait for Components.
//!
//! This trait defines the interface for Components to emit events.
//! The concrete implementation is provided by `orcs-runtime`.
//!
//! # Usage
//!
//! ```ignore
//! impl Component for MyComponent {
//!     fn set_emitter(&mut self, emitter: Box<dyn Emitter>) {
//!         self.emitter = Some(emitter);
//!     }
//! }
//!
//! // Later, emit output
//! if let Some(emitter) = &self.emitter {
//!     emitter.emit_output("Task completed");
//! }
//! ```

use std::fmt::Debug;

/// Trait for emitting events from Components.
///
/// Components receive an implementation of this trait via
/// [`Component::set_emitter`](crate::Component::set_emitter).
///
/// The emitter allows Components to:
/// - Send output to the user (via IO)
/// - Broadcast signals to other Components
pub trait Emitter: Send + Sync + Debug {
    /// Emits an output message (info level).
    ///
    /// The message will be displayed to the user via IOBridge.
    fn emit_output(&self, message: &str);

    /// Emits an output message with a specific level.
    ///
    /// # Arguments
    ///
    /// * `message` - The message to display
    /// * `level` - Log level ("info", "warn", "error")
    fn emit_output_with_level(&self, message: &str, level: &str);

    /// Emits a custom event (broadcast to all channels).
    ///
    /// Creates an Extension event with the given category and broadcasts
    /// it to all registered channels. Channels subscribed to the matching
    /// Extension category will process it.
    ///
    /// # Arguments
    ///
    /// * `category` - Extension kind string (e.g., "tool:result")
    /// * `operation` - Operation name (e.g., "complete")
    /// * `payload` - Event payload data
    ///
    /// # Returns
    ///
    /// `true` if the event was broadcast successfully.
    fn emit_event(&self, _category: &str, _operation: &str, _payload: serde_json::Value) -> bool {
        false
    }

    /// Returns the most recent `n` Board entries as JSON values.
    ///
    /// The Board is a shared rolling buffer of recent Output and Extension
    /// events. Components can query it to see what other components have
    /// emitted recently.
    ///
    /// Default implementation returns an empty vec (no board attached).
    ///
    /// # Arguments
    ///
    /// * `n` - Maximum number of entries to return
    fn board_recent(&self, _n: usize) -> Vec<serde_json::Value> {
        Vec::new()
    }

    /// Sends a synchronous RPC request to another Component.
    ///
    /// Routes via EventBus to the target Component's `on_request` handler
    /// and returns the response. Blocks the calling thread until response
    /// is received or timeout expires.
    ///
    /// # Arguments
    ///
    /// * `target` - Target Component name (e.g., "skill-manager")
    /// * `operation` - Operation name (e.g., "list", "activate")
    /// * `payload` - Request payload
    /// * `timeout_ms` - Optional timeout override (default: 30s)
    ///
    /// # Returns
    ///
    /// Response value from the target Component, or error string.
    ///
    /// # Default Implementation
    ///
    /// Returns error (not supported without runtime wiring).
    fn request(
        &self,
        _target: &str,
        _operation: &str,
        _payload: serde_json::Value,
        _timeout_ms: Option<u64>,
    ) -> Result<serde_json::Value, String> {
        Err("request not supported".into())
    }

    /// Checks whether a target Component is alive (its runner is still running).
    ///
    /// Uses the EventBus channel handle to determine liveness without sending
    /// a request. This is a best-effort check — the component may die
    /// immediately after returning `true` (TOCTOU inherent in concurrent systems).
    ///
    /// # Arguments
    ///
    /// * `target_fqn` - Fully qualified name or short name of the target Component
    ///
    /// # Default Implementation
    ///
    /// Returns `false` (no runtime wiring available).
    fn is_alive(&self, _target_fqn: &str) -> bool {
        false
    }

    /// Clones the emitter into a boxed trait object.
    ///
    /// This allows storing the emitter in the Component.
    fn clone_box(&self) -> Box<dyn Emitter>;
}

impl Clone for Box<dyn Emitter> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone)]
    struct MockEmitter;

    impl Emitter for MockEmitter {
        fn emit_output(&self, _message: &str) {
            // No-op for testing
        }

        fn emit_output_with_level(&self, _message: &str, _level: &str) {
            // No-op for testing
        }

        fn clone_box(&self) -> Box<dyn Emitter> {
            Box::new(self.clone())
        }
    }

    #[test]
    fn mock_emitter_works() {
        let emitter: Box<dyn Emitter> = Box::new(MockEmitter);
        emitter.emit_output("hello");
        emitter.emit_output_with_level("warning", "warn");
    }

    #[test]
    fn emitter_clone() {
        let emitter: Box<dyn Emitter> = Box::new(MockEmitter);
        let cloned = emitter.clone();
        cloned.emit_output("from clone");
    }
}

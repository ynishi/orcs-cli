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
    struct MockEmitter {
        name: String,
    }

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
        let emitter: Box<dyn Emitter> = Box::new(MockEmitter {
            name: "test".to_string(),
        });
        emitter.emit_output("hello");
        emitter.emit_output_with_level("warning", "warn");
    }

    #[test]
    fn emitter_clone() {
        let emitter: Box<dyn Emitter> = Box::new(MockEmitter {
            name: "test".to_string(),
        });
        let cloned = emitter.clone();
        cloned.emit_output("from clone");
    }
}

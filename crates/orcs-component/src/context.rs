//! Child context for runtime interaction.
//!
//! Provides the interface between Child entities and the Runner.
//! This enables Children to:
//!
//! - Emit output to the parent Component/IO
//! - Spawn sub-children
//! - Access runtime services safely
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    ComponentRunner (Rust)                    │
//! │                                                              │
//! │  ┌─────────────────────────────────────────────────────┐    │
//! │  │              ChildContextImpl                        │    │
//! │  │  - emit_output()     → event_tx                     │    │
//! │  │  - spawn_child()     → ChildSpawner                 │    │
//! │  │  - signal broadcast  → signal_tx                    │    │
//! │  └─────────────────────────────────────────────────────┘    │
//! └─────────────────────────────────────────────────────────────┘
//!                              │
//!                              │ inject via set_context()
//!                              ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │                     Child (Lua/Rust)                         │
//! │                                                              │
//! │  fn run(&mut self, input: Value) -> ChildResult {           │
//! │      self.ctx.emit_output("Starting work...");              │
//! │      let sub = self.ctx.spawn_child(config)?;               │
//! │      // ...                                                  │
//! │  }                                                           │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Safety
//!
//! The context provides a safe, controlled interface to runtime services.
//! Children cannot directly access the EventBus or other internal systems.

use crate::ChildResult;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::path::PathBuf;
use thiserror::Error;

/// Error when spawning a child fails.
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
pub enum SpawnError {
    /// Maximum number of children reached.
    #[error("max children limit reached: {0}")]
    MaxChildrenReached(usize),

    /// Script file not found.
    #[error("script not found: {0}")]
    ScriptNotFound(String),

    /// Invalid script content.
    #[error("invalid script: {0}")]
    InvalidScript(String),

    /// Child with same ID already exists.
    #[error("child already exists: {0}")]
    AlreadyExists(String),

    /// Internal error (lock poisoned, channel closed, etc.)
    #[error("internal error: {0}")]
    Internal(String),
}

/// Error when running a child fails.
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
pub enum RunError {
    /// Child not found.
    #[error("child not found: {0}")]
    NotFound(String),

    /// Child is not runnable.
    #[error("child not runnable: {0}")]
    NotRunnable(String),

    /// Execution failed.
    #[error("execution failed: {0}")]
    ExecutionFailed(String),

    /// Child was aborted.
    #[error("child aborted")]
    Aborted,
}

/// Configuration for spawning a child.
///
/// # Example
///
/// ```
/// use orcs_component::ChildConfig;
/// use std::path::PathBuf;
///
/// // From file
/// let config = ChildConfig::from_file("worker-1", "scripts/worker.lua");
///
/// // From inline script
/// let config = ChildConfig::from_inline("worker-2", r#"
///     return {
///         run = function(input) return input end,
///         on_signal = function(sig) return "Handled" end,
///     }
/// "#);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildConfig {
    /// Unique identifier for the child.
    pub id: String,

    /// Path to the script file (for Lua children).
    pub script_path: Option<PathBuf>,

    /// Inline script content (for Lua children).
    pub script_inline: Option<String>,
}

impl ChildConfig {
    /// Creates a config for a child from a script file.
    #[must_use]
    pub fn from_file(id: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            id: id.into(),
            script_path: Some(path.into()),
            script_inline: None,
        }
    }

    /// Creates a config for a child from inline script.
    #[must_use]
    pub fn from_inline(id: impl Into<String>, script: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            script_path: None,
            script_inline: Some(script.into()),
        }
    }

    /// Creates a minimal config with just an ID.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            script_path: None,
            script_inline: None,
        }
    }
}

/// Handle to a spawned child (synchronous).
///
/// Provides control over a child that was spawned via
/// [`ChildContext::spawn_child()`].
///
/// For async operations, see [`AsyncChildHandle`].
///
/// # Example
///
/// ```ignore
/// let mut handle = ctx.spawn_child(config)?;
///
/// // Run the child (blocking)
/// let result = handle.run_sync(input)?;
///
/// // Check status
/// println!("Status: {:?}", handle.status());
///
/// // Abort if needed
/// handle.abort();
/// ```
pub trait ChildHandle: Send + Sync + Debug {
    /// Returns the child's ID.
    fn id(&self) -> &str;

    /// Returns the child's current status.
    fn status(&self) -> crate::Status;

    /// Runs the child with the given input (blocking).
    ///
    /// # Arguments
    ///
    /// * `input` - Input data for the child
    ///
    /// # Returns
    ///
    /// The result of the child's execution.
    fn run_sync(&mut self, input: serde_json::Value) -> Result<ChildResult, RunError>;

    /// Aborts the child immediately.
    fn abort(&mut self);

    /// Returns `true` if the child has completed (success, error, or aborted).
    fn is_finished(&self) -> bool;
}

/// Async handle to a spawned child.
///
/// Provides async control over a child. Use this for children that
/// perform I/O-bound operations like LLM API calls.
///
/// # Example
///
/// ```ignore
/// let mut handle = ctx.spawn_child_async(config).await?;
///
/// // Run the child (async)
/// let result = handle.run(input).await?;
///
/// // Check status
/// println!("Status: {:?}", handle.status());
///
/// // Abort if needed
/// handle.abort();
/// ```
///
/// # When to Use
///
/// | Handle | Use Case |
/// |--------|----------|
/// | `ChildHandle` | CPU-bound, quick sync tasks |
/// | `AsyncChildHandle` | I/O-bound, network, LLM calls |
#[async_trait]
pub trait AsyncChildHandle: Send + Sync + Debug {
    /// Returns the child's ID.
    fn id(&self) -> &str;

    /// Returns the child's current status.
    fn status(&self) -> crate::Status;

    /// Runs the child with the given input (async).
    ///
    /// # Arguments
    ///
    /// * `input` - Input data for the child
    ///
    /// # Returns
    ///
    /// The result of the child's execution.
    async fn run(&mut self, input: serde_json::Value) -> Result<ChildResult, RunError>;

    /// Aborts the child immediately.
    fn abort(&mut self);

    /// Returns `true` if the child has completed (success, error, or aborted).
    fn is_finished(&self) -> bool;
}

/// Context provided to Children for runtime interaction.
///
/// This trait defines the safe interface that Children can use
/// to interact with the runtime. Implementations are provided
/// by the Runner.
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync` to allow Children
/// to hold references across thread boundaries.
///
/// # Example
///
/// ```ignore
/// struct MyChild {
///     id: String,
///     ctx: Option<Box<dyn ChildContext>>,
/// }
///
/// impl RunnableChild for MyChild {
///     fn run(&mut self, input: Value) -> ChildResult {
///         if let Some(ctx) = &self.ctx {
///             ctx.emit_output("Starting work...");
///
///             // Spawn a sub-child
///             let config = ChildConfig::from_inline("sub-1", "...");
///             if let Ok(mut handle) = ctx.spawn_child(config) {
///                 let sub_result = handle.run_sync(input.clone());
///                 // ...
///             }
///         }
///         ChildResult::Ok(input)
///     }
/// }
/// ```
pub trait ChildContext: Send + Sync + Debug {
    /// Returns the parent's ID (Component or Child that owns this context).
    fn parent_id(&self) -> &str;

    /// Emits output to the parent (displayed to user via IO).
    ///
    /// # Arguments
    ///
    /// * `message` - Message to display
    fn emit_output(&self, message: &str);

    /// Emits output with a specific level.
    ///
    /// # Arguments
    ///
    /// * `message` - Message to display
    /// * `level` - Log level ("info", "warn", "error")
    fn emit_output_with_level(&self, message: &str, level: &str);

    /// Spawns a child and returns a sync handle to control it.
    ///
    /// For async spawning, see [`AsyncChildContext::spawn_child`].
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration for the child
    ///
    /// # Returns
    ///
    /// A handle to the spawned child, or an error if spawn failed.
    ///
    /// # Errors
    ///
    /// - [`SpawnError::MaxChildrenReached`] if limit exceeded
    /// - [`SpawnError::ScriptNotFound`] if script file doesn't exist
    /// - [`SpawnError::InvalidScript`] if script is malformed
    /// - [`SpawnError::AlreadyExists`] if ID is already in use
    fn spawn_child(&self, config: ChildConfig) -> Result<Box<dyn ChildHandle>, SpawnError>;

    /// Returns the number of active children.
    fn child_count(&self) -> usize;

    /// Returns the maximum allowed children.
    fn max_children(&self) -> usize;

    /// Clones this context into a boxed trait object.
    fn clone_box(&self) -> Box<dyn ChildContext>;
}

impl Clone for Box<dyn ChildContext> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

/// Async context provided to Children for runtime interaction.
///
/// This trait provides async versions of [`ChildContext`] methods
/// for children that need to spawn async children.
///
/// # Example
///
/// ```ignore
/// #[async_trait]
/// impl AsyncRunnableChild for MyWorker {
///     async fn run(&mut self, input: Value) -> ChildResult {
///         if let Some(ctx) = &self.async_ctx {
///             ctx.emit_output("Starting async work...");
///
///             // Spawn an async child
///             let config = ChildConfig::from_inline("sub-1", "...");
///             if let Ok(mut handle) = ctx.spawn_child(config).await {
///                 let result = handle.run(input.clone()).await;
///                 // ...
///             }
///         }
///         ChildResult::Ok(input)
///     }
/// }
/// ```
#[async_trait]
pub trait AsyncChildContext: Send + Sync + Debug {
    /// Returns the parent's ID (Component or Child that owns this context).
    fn parent_id(&self) -> &str;

    /// Emits output to the parent (displayed to user via IO).
    fn emit_output(&self, message: &str);

    /// Emits output with a specific level.
    fn emit_output_with_level(&self, message: &str, level: &str);

    /// Spawns a child and returns an async handle to control it.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration for the child
    ///
    /// # Returns
    ///
    /// An async handle to the spawned child, or an error if spawn failed.
    ///
    /// # Errors
    ///
    /// - [`SpawnError::MaxChildrenReached`] if limit exceeded
    /// - [`SpawnError::ScriptNotFound`] if script file doesn't exist
    /// - [`SpawnError::InvalidScript`] if script is malformed
    /// - [`SpawnError::AlreadyExists`] if ID is already in use
    async fn spawn_child(
        &self,
        config: ChildConfig,
    ) -> Result<Box<dyn AsyncChildHandle>, SpawnError>;

    /// Returns the number of active children.
    fn child_count(&self) -> usize;

    /// Returns the maximum allowed children.
    fn max_children(&self) -> usize;

    /// Clones this context into a boxed trait object.
    fn clone_box(&self) -> Box<dyn AsyncChildContext>;
}

impl Clone for Box<dyn AsyncChildContext> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_config_from_file() {
        let config = ChildConfig::from_file("worker", "scripts/worker.lua");
        assert_eq!(config.id, "worker");
        assert_eq!(
            config.script_path,
            Some(PathBuf::from("scripts/worker.lua"))
        );
        assert!(config.script_inline.is_none());
    }

    #[test]
    fn child_config_from_inline() {
        let config = ChildConfig::from_inline("inline-worker", "return {}");
        assert_eq!(config.id, "inline-worker");
        assert!(config.script_path.is_none());
        assert_eq!(config.script_inline, Some("return {}".into()));
    }

    #[test]
    fn child_config_new() {
        let config = ChildConfig::new("minimal");
        assert_eq!(config.id, "minimal");
        assert!(config.script_path.is_none());
        assert!(config.script_inline.is_none());
    }

    #[test]
    fn spawn_error_display() {
        let err = SpawnError::MaxChildrenReached(10);
        assert!(err.to_string().contains("10"));

        let err = SpawnError::ScriptNotFound("test.lua".into());
        assert!(err.to_string().contains("test.lua"));

        let err = SpawnError::AlreadyExists("worker-1".into());
        assert!(err.to_string().contains("worker-1"));
    }

    #[test]
    fn run_error_display() {
        let err = RunError::NotFound("child-1".into());
        assert!(err.to_string().contains("child-1"));

        let err = RunError::Aborted;
        assert!(err.to_string().contains("aborted"));
    }

    #[test]
    fn child_config_serialize() {
        let config = ChildConfig::from_file("test", "test.lua");
        let json = serde_json::to_string(&config).expect("serialize");
        assert!(json.contains("test"));

        let config2: ChildConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(config2.id, "test");
    }

    // --- Mock implementations for testing ---

    #[derive(Debug)]
    struct MockChildHandle {
        id: String,
        status: crate::Status,
    }

    impl ChildHandle for MockChildHandle {
        fn id(&self) -> &str {
            &self.id
        }

        fn status(&self) -> crate::Status {
            self.status
        }

        fn run_sync(&mut self, input: serde_json::Value) -> Result<ChildResult, RunError> {
            self.status = crate::Status::Running;
            // Simulate work
            self.status = crate::Status::Idle;
            Ok(ChildResult::Ok(input))
        }

        fn abort(&mut self) {
            self.status = crate::Status::Aborted;
        }

        fn is_finished(&self) -> bool {
            self.status.is_terminal()
        }
    }

    #[test]
    fn child_handle_run_sync() {
        let mut handle = MockChildHandle {
            id: "test-child".into(),
            status: crate::Status::Idle,
        };

        let input = serde_json::json!({"key": "value"});
        let result = handle.run_sync(input.clone());

        assert!(result.is_ok());
        if let Ok(ChildResult::Ok(value)) = result {
            assert_eq!(value, input);
        }
    }

    #[test]
    fn child_handle_abort() {
        let mut handle = MockChildHandle {
            id: "test-child".into(),
            status: crate::Status::Running,
        };

        handle.abort();
        assert_eq!(handle.status(), crate::Status::Aborted);
        assert!(handle.is_finished());
    }

    #[derive(Debug)]
    struct MockAsyncChildHandle {
        id: String,
        status: crate::Status,
    }

    #[async_trait]
    impl AsyncChildHandle for MockAsyncChildHandle {
        fn id(&self) -> &str {
            &self.id
        }

        fn status(&self) -> crate::Status {
            self.status
        }

        async fn run(&mut self, input: serde_json::Value) -> Result<ChildResult, RunError> {
            self.status = crate::Status::Running;
            // Simulate async work
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            self.status = crate::Status::Idle;
            Ok(ChildResult::Ok(input))
        }

        fn abort(&mut self) {
            self.status = crate::Status::Aborted;
        }

        fn is_finished(&self) -> bool {
            self.status.is_terminal()
        }
    }

    #[tokio::test]
    async fn async_child_handle_run() {
        let mut handle = MockAsyncChildHandle {
            id: "async-test-child".into(),
            status: crate::Status::Idle,
        };

        let input = serde_json::json!({"async": true});
        let result = handle.run(input.clone()).await;

        assert!(result.is_ok());
        if let Ok(ChildResult::Ok(value)) = result {
            assert_eq!(value, input);
        }
    }

    #[tokio::test]
    async fn async_child_handle_object_safety() {
        let handle: Box<dyn AsyncChildHandle> = Box::new(MockAsyncChildHandle {
            id: "async-boxed".into(),
            status: crate::Status::Idle,
        });

        assert_eq!(handle.id(), "async-boxed");
        assert_eq!(handle.status(), crate::Status::Idle);
    }
}

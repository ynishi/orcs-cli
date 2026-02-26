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

use crate::capability::Capability;
use crate::ChildResult;
use async_trait::async_trait;
use orcs_types::ChannelId;
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

    /// Permission denied.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

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

/// Loader for creating Components from scripts.
///
/// This trait allows runtime implementations to create Components
/// without depending on specific implementations (e.g., LuaComponent).
pub trait ComponentLoader: Send + Sync {
    /// Creates a Component from inline script content.
    ///
    /// # Arguments
    ///
    /// * `script` - Inline script content
    /// * `id` - Optional component ID (extracted from script if None)
    /// * `globals` - Optional key-value pairs to inject into the VM before script
    ///   execution. Each entry becomes a global variable in the new VM, enabling
    ///   structured data passing without string-based code injection.
    ///
    ///   The type is `Map<String, Value>` (a JSON object) rather than a generic
    ///   `serde_json::Value` so that the compiler enforces the "must be an object"
    ///   invariant at the call site. This follows the *parse, don't validate*
    ///   principle — callers convert to `Map` once, and downstream code never
    ///   needs to re-check or use `unreachable!()` branches.
    ///
    /// # Returns
    ///
    /// A boxed Component, or an error if loading failed.
    fn load_from_script(
        &self,
        script: &str,
        id: Option<&str>,
        globals: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<Box<dyn crate::Component>, SpawnError>;

    /// Resolves a builtin component name to its script content.
    ///
    /// # Default Implementation
    ///
    /// Returns `None` (no builtins available).
    fn resolve_builtin(&self, _name: &str) -> Option<String> {
        None
    }
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

    /// Requested capabilities for the child.
    ///
    /// - `None` — inherit all of the parent's capabilities (default).
    /// - `Some(caps)` — request specific capabilities. The effective set
    ///   is `parent_caps & requested_caps` (a child cannot exceed its parent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Capability>,
}

impl ChildConfig {
    /// Creates a config for a child from a script file.
    #[must_use]
    pub fn from_file(id: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            id: id.into(),
            script_path: Some(path.into()),
            script_inline: None,
            capabilities: None,
        }
    }

    /// Creates a config for a child from inline script.
    #[must_use]
    pub fn from_inline(id: impl Into<String>, script: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            script_path: None,
            script_inline: Some(script.into()),
            capabilities: None,
        }
    }

    /// Creates a minimal config with just an ID.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            script_path: None,
            script_inline: None,
            capabilities: None,
        }
    }

    /// Sets the requested capabilities for this child.
    ///
    /// The effective capabilities will be `parent_caps & requested_caps`.
    #[must_use]
    pub fn with_capabilities(mut self, caps: Capability) -> Self {
        self.capabilities = Some(caps);
        self
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

// Re-export from orcs-auth for backward compatibility
pub use orcs_auth::CommandPermission;

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

    /// Emits an approval request for HIL flow.
    ///
    /// Generates a unique approval ID and emits the request to the output
    /// channel so it can be displayed to the user.
    ///
    /// # Returns
    ///
    /// The generated approval ID that can be matched in `on_signal`.
    ///
    /// # Default Implementation
    ///
    /// Returns empty string (no-op for backward compatibility).
    fn emit_approval_request(&self, _operation: &str, _description: &str) -> String {
        String::new()
    }

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

    /// Sends input to a child by ID and returns its result.
    ///
    /// # Arguments
    ///
    /// * `child_id` - The child's ID
    /// * `input` - Input data to pass to the child
    ///
    /// # Returns
    ///
    /// The child's result.
    ///
    /// # Errors
    ///
    /// - [`RunError::NotFound`] if child doesn't exist
    /// - [`RunError::ExecutionFailed`] if child execution fails
    fn send_to_child(
        &self,
        child_id: &str,
        input: serde_json::Value,
    ) -> Result<ChildResult, RunError>;

    /// Sends input to a child asynchronously (fire-and-forget).
    ///
    /// Unlike [`send_to_child`](Self::send_to_child), this method returns immediately without
    /// waiting for the child to complete. The child runs in a background
    /// thread and its output flows through `emit_output` automatically.
    ///
    /// # Arguments
    ///
    /// * `child_id` - The child's ID
    /// * `input` - Input data to pass to the child
    ///
    /// # Returns
    ///
    /// `Ok(())` if the child was found and background execution started.
    ///
    /// # Errors
    ///
    /// - [`RunError::NotFound`] if child doesn't exist
    /// - [`RunError::ExecutionFailed`] if spawner lock fails
    ///
    /// # Default Implementation
    ///
    /// Returns an error indicating async send is not supported.
    fn send_to_child_async(
        &self,
        _child_id: &str,
        _input: serde_json::Value,
    ) -> Result<(), RunError> {
        Err(RunError::ExecutionFailed(
            "send_to_child_async not supported by this context".into(),
        ))
    }

    /// Sends input to multiple children in parallel and returns all results.
    ///
    /// Each `(child_id, input)` pair is executed concurrently using OS threads.
    /// The spawner lock is held only briefly to collect child references, then
    /// released before execution begins.
    ///
    /// # Arguments
    ///
    /// * `requests` - Vec of `(child_id, input)` pairs
    ///
    /// # Returns
    ///
    /// Vec of `(child_id, Result<ChildResult, RunError>)` in the same order.
    ///
    /// # Default Implementation
    ///
    /// Falls back to sequential `send_to_child` calls.
    fn send_to_children_batch(
        &self,
        requests: Vec<(String, serde_json::Value)>,
    ) -> Vec<(String, Result<ChildResult, RunError>)> {
        requests
            .into_iter()
            .map(|(id, input)| {
                let result = self.send_to_child(&id, input);
                (id, result)
            })
            .collect()
    }

    /// Spawns a Component as a separate ChannelRunner from a script.
    ///
    /// This creates a new Channel in the World and spawns a ChannelRunner
    /// to execute the Component in parallel. The Component is created
    /// from the provided script content.
    ///
    /// # Arguments
    ///
    /// * `script` - Inline script content (e.g., Lua component script)
    /// * `id` - Optional component ID (extracted from script if None)
    /// * `globals` - Optional key-value pairs to inject into the VM. See
    ///   [`ComponentLoader::load_from_script`] for the rationale behind using
    ///   `Map<String, Value>` instead of a generic `serde_json::Value`.
    ///
    /// # Returns
    ///
    /// A tuple of `(ChannelId, fqn_string)` for the spawned runner,
    /// or an error if spawning failed. The FQN enables immediate
    /// `orcs.request(fqn, ...)` communication.
    ///
    /// # Default Implementation
    ///
    /// Returns `SpawnError::Internal` indicating runner spawning is not supported.
    /// Implementations that support runner spawning should override this.
    fn spawn_runner_from_script(
        &self,
        _script: &str,
        _id: Option<&str>,
        _globals: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<(ChannelId, String), SpawnError> {
        Err(SpawnError::Internal(
            "runner spawning not supported by this context".into(),
        ))
    }

    /// Spawns a ChannelRunner from a builtin component name.
    ///
    /// Resolves the builtin name to script content via the component loader,
    /// then delegates to [`spawn_runner_from_script`](Self::spawn_runner_from_script).
    ///
    /// # Default Implementation
    ///
    /// Returns `SpawnError::Internal` indicating builtin spawning is not supported.
    fn spawn_runner_from_builtin(
        &self,
        _name: &str,
        _id: Option<&str>,
    ) -> Result<(ChannelId, String), SpawnError> {
        Err(SpawnError::Internal(
            "builtin spawning not supported by this context".into(),
        ))
    }

    /// Returns the capabilities granted to this context.
    ///
    /// # Default Implementation
    ///
    /// Returns [`Capability::ALL`] (permissive mode for backward compatibility).
    /// Override this to restrict the capabilities available to children.
    fn capabilities(&self) -> Capability {
        Capability::ALL
    }

    /// Checks if this context has a specific capability.
    ///
    /// Equivalent to `self.capabilities().contains(cap)`.
    fn has_capability(&self, cap: Capability) -> bool {
        self.capabilities().contains(cap)
    }

    /// Checks if command execution is allowed.
    ///
    /// # Default Implementation
    ///
    /// Returns `true` (permissive mode for backward compatibility).
    /// Implementations with session/checker support should override this.
    fn can_execute_command(&self, _cmd: &str) -> bool {
        true
    }

    /// Checks command with granular permission result.
    ///
    /// Returns [`CommandPermission`] with three possible states:
    /// - `Allowed`: Execute immediately
    /// - `Denied`: Block with reason
    /// - `RequiresApproval`: Needs user approval before execution
    ///
    /// # Default Implementation
    ///
    /// Returns `Allowed` (permissive mode for backward compatibility).
    fn check_command_permission(&self, _cmd: &str) -> CommandPermission {
        CommandPermission::Allowed
    }

    /// Checks if a command pattern has been granted (without elevation bypass).
    ///
    /// Returns `true` if the command matches a previously granted pattern.
    /// Unlike [`check_command_permission`](Self::check_command_permission),
    /// this does NOT consider session elevation — only explicit grants.
    ///
    /// # Default Implementation
    ///
    /// Returns `false` (no grants in permissive mode).
    fn is_command_granted(&self, _cmd: &str) -> bool {
        false
    }

    /// Grants a command pattern for future execution.
    ///
    /// After HIL approval, call this to allow matching commands
    /// without re-approval.
    ///
    /// # Default Implementation
    ///
    /// No-op (for backward compatibility).
    fn grant_command(&self, _pattern: &str) {}

    /// Checks if child spawning is allowed.
    ///
    /// # Default Implementation
    ///
    /// Returns `true` (permissive mode for backward compatibility).
    fn can_spawn_child_auth(&self) -> bool {
        true
    }

    /// Checks if runner spawning is allowed.
    ///
    /// # Default Implementation
    ///
    /// Returns `true` (permissive mode for backward compatibility).
    fn can_spawn_runner_auth(&self) -> bool {
        true
    }

    /// Sends an RPC request to another Component by FQN.
    ///
    /// # Arguments
    ///
    /// * `target_fqn` - FQN of the target component (e.g. `"skill::skill_manager"`)
    /// * `operation` - The operation to invoke (e.g. `"list"`, `"catalog"`)
    /// * `payload` - JSON payload for the operation
    /// * `timeout_ms` - Optional timeout in milliseconds
    ///
    /// # Returns
    ///
    /// The response value from the target component.
    ///
    /// # Default Implementation
    ///
    /// Returns an error indicating RPC is not supported.
    fn request(
        &self,
        _target_fqn: &str,
        _operation: &str,
        _payload: serde_json::Value,
        _timeout_ms: Option<u64>,
    ) -> Result<serde_json::Value, String> {
        Err("request not supported by this context".into())
    }

    /// Sends multiple RPC requests in parallel and returns all results.
    ///
    /// Each request is a tuple of `(target_fqn, operation, payload, timeout_ms)`.
    /// Results are returned in the same order as the input.
    ///
    /// # Default Implementation
    ///
    /// Falls back to sequential `request()` calls.
    fn request_batch(
        &self,
        requests: Vec<(String, String, serde_json::Value, Option<u64>)>,
    ) -> Vec<Result<serde_json::Value, String>> {
        requests
            .into_iter()
            .map(|(target, op, payload, timeout)| self.request(&target, &op, payload, timeout))
            .collect()
    }

    /// Returns a type-erased runtime extension by key.
    ///
    /// Enables runtime-layer constructs (e.g., hook registries) to pass
    /// through the Plugin SDK layer without introducing layer-breaking
    /// dependencies.
    ///
    /// # Default Implementation
    ///
    /// Returns `None` (no extensions available).
    fn extension(&self, _key: &str) -> Option<Box<dyn std::any::Any + Send + Sync>> {
        None
    }

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

    /// Sends input to a child by ID and returns its result.
    ///
    /// # Arguments
    ///
    /// * `child_id` - The child's ID
    /// * `input` - Input data to pass to the child
    fn send_to_child(
        &self,
        child_id: &str,
        input: serde_json::Value,
    ) -> Result<ChildResult, RunError>;

    /// Sends input to a child asynchronously (fire-and-forget).
    ///
    /// Returns immediately. The child runs in a background thread.
    ///
    /// # Default Implementation
    ///
    /// Returns an error indicating async send is not supported.
    fn send_to_child_async(
        &self,
        _child_id: &str,
        _input: serde_json::Value,
    ) -> Result<(), RunError> {
        Err(RunError::ExecutionFailed(
            "send_to_child_async not supported by this context".into(),
        ))
    }

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

    // --- CommandPermission tests ---

    #[test]
    fn command_permission_allowed() {
        let p = CommandPermission::Allowed;
        assert!(p.is_allowed());
        assert!(!p.is_denied());
        assert!(!p.requires_approval());
        assert_eq!(p.status_str(), "allowed");
    }

    #[test]
    fn command_permission_denied() {
        let p = CommandPermission::Denied("blocked".to_string());
        assert!(!p.is_allowed());
        assert!(p.is_denied());
        assert!(!p.requires_approval());
        assert_eq!(p.status_str(), "denied");
    }

    #[test]
    fn command_permission_requires_approval() {
        let p = CommandPermission::RequiresApproval {
            grant_pattern: "rm -rf".to_string(),
            description: "destructive operation".to_string(),
        };
        assert!(!p.is_allowed());
        assert!(!p.is_denied());
        assert!(p.requires_approval());
        assert_eq!(p.status_str(), "requires_approval");
    }

    #[test]
    fn command_permission_eq() {
        assert_eq!(CommandPermission::Allowed, CommandPermission::Allowed);
        assert_eq!(
            CommandPermission::Denied("x".into()),
            CommandPermission::Denied("x".into())
        );
        assert_ne!(
            CommandPermission::Allowed,
            CommandPermission::Denied("x".into())
        );
    }
}

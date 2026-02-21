//! Child trait for component-managed entities.
//!
//! Components may internally manage child entities. All children
//! must implement this trait to ensure proper signal handling.
//!
//! # Why Child Trait?
//!
//! Without this constraint, Components could hold arbitrary entities
//! that don't respond to signals. This would break the "Human as
//! Superpower" principle - Veto signals must reach everything.
//!
//! # Child Hierarchy
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                     Child (base trait)                       │
//! │  - Identifiable + SignalReceiver + Statusable               │
//! │  - Passive: managed by Component                             │
//! └─────────────────────────────────────────────────────────────┘
//!                              │
//!                              ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │                   RunnableChild (extends Child)              │
//! │  - Active: can execute work via run()                        │
//! │  - Used for SubAgents, Workers, Skills                       │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Component-Child Relationship
//!
//! ```text
//! ┌─────────────────────────────────────────────┐
//! │            Component (e.g., LlmComponent)    │
//! │                                              │
//! │  on_signal(Signal) {                        │
//! │      // Forward to all children             │
//! │      for child in &mut self.children {      │
//! │          child.on_signal(&signal);          │
//! │      }                                       │
//! │  }                                           │
//! │                                              │
//! │  children: Vec<Box<dyn Child>>              │
//! │  ┌────────┐  ┌────────┐  ┌────────┐        │
//! │  │ Agent1 │  │ Agent2 │  │ Worker │        │
//! │  │impl    │  │impl    │  │impl    │        │
//! │  │ Child  │  │ Child  │  │ Child  │        │
//! │  └────────┘  └────────┘  └────────┘        │
//! └─────────────────────────────────────────────┘
//! ```
//!
//! # Domain-Specific Implementations
//!
//! Concrete child types are defined in domain crates:
//!
//! - `orcs-llm`: `Agent impl Child`
//! - `orcs-skill`: `Skill impl Child`
//! - etc.
//!
//! # Example
//!
//! ```
//! use orcs_component::{Child, Identifiable, SignalReceiver, Statusable, Status};
//! use orcs_event::{Signal, SignalResponse};
//!
//! // Domain-specific child implementation
//! struct MyAgent {
//!     id: String,
//!     status: Status,
//! }
//!
//! impl Identifiable for MyAgent {
//!     fn id(&self) -> &str {
//!         &self.id
//!     }
//! }
//!
//! impl SignalReceiver for MyAgent {
//!     fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
//!         if signal.is_veto() {
//!             self.abort();
//!             SignalResponse::Abort
//!         } else {
//!             SignalResponse::Ignored
//!         }
//!     }
//!
//!     fn abort(&mut self) {
//!         self.status = Status::Aborted;
//!     }
//! }
//!
//! impl Statusable for MyAgent {
//!     fn status(&self) -> Status {
//!         self.status
//!     }
//! }
//!
//! // Mark as Child - now safe to hold in Component
//! impl Child for MyAgent {}
//! ```

use crate::{Identifiable, SignalReceiver, Statusable};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

// ============================================
// Internal Error Type (thiserror-based)
// ============================================

/// Child execution errors.
///
/// Used internally for type-safe error handling.
/// Convert to [`ChildResultDto`] for serialization.
#[derive(Debug, Clone, Error)]
pub enum ChildError {
    /// Execution failed with a reason.
    #[error("execution failed: {reason}")]
    ExecutionFailed { reason: String },

    /// Input validation failed.
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// Timeout during execution.
    #[error("timeout after {elapsed_ms}ms")]
    Timeout { elapsed_ms: u64 },

    /// Internal error.
    #[error("internal: {0}")]
    Internal(String),
}

impl ChildError {
    /// Returns the error kind as a string identifier.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ExecutionFailed { .. } => "execution_failed",
            Self::InvalidInput(_) => "invalid_input",
            Self::Timeout { .. } => "timeout",
            Self::Internal(_) => "internal",
        }
    }
}

/// A child entity managed by a Component.
///
/// All entities held inside a Component must implement this trait.
/// This ensures they can:
///
/// - Be identified ([`Identifiable`])
/// - Respond to signals ([`SignalReceiver`])
/// - Report status ([`Statusable`])
///
/// # Object Safety
///
/// This trait is object-safe, allowing `Box<dyn Child>`.
///
/// # Signal Propagation
///
/// When a Component receives a signal, it MUST forward it to all children:
///
/// ```ignore
/// fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
///     // Handle self first
///     // ...
///
///     // Forward to ALL children
///     for child in &mut self.children {
///         child.on_signal(signal);
///     }
/// }
/// ```
///
/// # Type Parameter Alternative
///
/// For stronger typing, Components can use:
///
/// ```ignore
/// struct MyComponent<C: Child> {
///     children: Vec<C>,
/// }
/// ```
pub trait Child: Identifiable + SignalReceiver + Statusable + Send + Sync {
    /// Inject a [`ChildContext`](crate::ChildContext) so the child can use `orcs.*` functions at
    /// runtime (RPC, exec, spawn, file tools, etc.).
    ///
    /// The default implementation is a no-op — override when the child
    /// actually needs context (e.g. `LuaChild`).
    fn set_context(&mut self, _ctx: Box<dyn super::ChildContext>) {}
}

// ============================================
// Internal Result Type
// ============================================

/// Result of a Child's work execution.
///
/// Represents the outcome of [`RunnableChild::run()`].
/// For serialization, convert to [`ChildResultDto`] using `.into()`.
///
/// # Variants
///
/// - `Ok`: Work completed successfully with optional output data
/// - `Err`: Work failed with a typed error
/// - `Aborted`: Work was interrupted by a Signal (Veto/Cancel)
///
/// # Example
///
/// ```
/// use orcs_component::{ChildResult, ChildError};
/// use serde_json::json;
///
/// let success = ChildResult::Ok(json!({"processed": true}));
/// assert!(success.is_ok());
///
/// let failure = ChildResult::Err(ChildError::Timeout { elapsed_ms: 5000 });
/// assert!(failure.is_err());
///
/// let aborted = ChildResult::Aborted;
/// assert!(aborted.is_aborted());
/// ```
#[derive(Debug, Clone)]
pub enum ChildResult {
    /// Work completed successfully.
    Ok(Value),
    /// Work failed with a typed error.
    Err(ChildError),
    /// Work was aborted by a Signal.
    Aborted,
}

impl ChildResult {
    /// Returns `true` if the result is `Ok`.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }

    /// Returns `true` if the result is `Err`.
    #[must_use]
    pub fn is_err(&self) -> bool {
        matches!(self, Self::Err(_))
    }

    /// Returns `true` if the result is `Aborted`.
    #[must_use]
    pub fn is_aborted(&self) -> bool {
        matches!(self, Self::Aborted)
    }

    /// Returns the success value if `Ok`, otherwise `None`.
    #[must_use]
    pub fn ok(self) -> Option<Value> {
        match self {
            Self::Ok(v) => Some(v),
            _ => None,
        }
    }

    /// Returns the error if `Err`, otherwise `None`.
    #[must_use]
    pub fn err(self) -> Option<ChildError> {
        match self {
            Self::Err(e) => Some(e),
            _ => None,
        }
    }
}

impl Default for ChildResult {
    fn default() -> Self {
        Self::Ok(Value::Null)
    }
}

impl From<Value> for ChildResult {
    fn from(value: Value) -> Self {
        Self::Ok(value)
    }
}

impl From<ChildError> for ChildResult {
    fn from(err: ChildError) -> Self {
        Self::Err(err)
    }
}

// ============================================
// External DTO Type (Serializable)
// ============================================

/// Serializable representation of [`ChildResult`].
///
/// Use this type for IPC, persistence, or JSON serialization.
///
/// # Example
///
/// ```
/// use orcs_component::{ChildResult, ChildResultDto, ChildError};
/// use serde_json::json;
///
/// let result = ChildResult::Err(ChildError::Timeout { elapsed_ms: 5000 });
/// let dto: ChildResultDto = result.into();
///
/// let json = serde_json::to_string(&dto).unwrap();
/// assert!(json.contains("timeout"));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ChildResultDto {
    /// Work completed successfully.
    Ok(Value),
    /// Work failed with error details.
    Err {
        /// Error kind identifier (e.g., "timeout", "invalid_input").
        kind: String,
        /// Human-readable error message.
        message: String,
    },
    /// Work was aborted by a Signal.
    Aborted,
}

impl ChildResultDto {
    /// Returns `true` if the result is `Ok`.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }

    /// Returns `true` if the result is `Err`.
    #[must_use]
    pub fn is_err(&self) -> bool {
        matches!(self, Self::Err { .. })
    }

    /// Returns `true` if the result is `Aborted`.
    #[must_use]
    pub fn is_aborted(&self) -> bool {
        matches!(self, Self::Aborted)
    }
}

impl From<ChildResult> for ChildResultDto {
    fn from(result: ChildResult) -> Self {
        match result {
            ChildResult::Ok(v) => Self::Ok(v),
            ChildResult::Err(e) => Self::Err {
                kind: e.kind().to_string(),
                message: e.to_string(),
            },
            ChildResult::Aborted => Self::Aborted,
        }
    }
}

impl Default for ChildResultDto {
    fn default() -> Self {
        Self::Ok(Value::Null)
    }
}

/// A Child that can actively execute work.
///
/// Extends [`Child`] with the ability to run tasks.
/// Used for SubAgents, Workers, and Skills that need to
/// perform actual work rather than just respond to signals.
///
/// # Architecture
///
/// ```text
/// Component (Manager)
///     │
///     │ spawn_child()
///     ▼
/// RunnableChild (Worker)
///     │
///     │ run(input)
///     ▼
/// ChildResult
/// ```
///
/// # Signal Handling
///
/// During `run()`, the child should periodically check for
/// signals and return `ChildResult::Aborted` if a Veto/Cancel
/// is received.
///
/// # Example
///
/// ```
/// use orcs_component::{Child, RunnableChild, ChildResult, Identifiable, SignalReceiver, Statusable, Status};
/// use orcs_event::{Signal, SignalResponse};
/// use serde_json::{json, Value};
///
/// struct Worker {
///     id: String,
///     status: Status,
/// }
///
/// impl Identifiable for Worker {
///     fn id(&self) -> &str { &self.id }
/// }
///
/// impl SignalReceiver for Worker {
///     fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
///         if signal.is_veto() {
///             self.status = Status::Aborted;
///             SignalResponse::Abort
///         } else {
///             SignalResponse::Handled
///         }
///     }
///     fn abort(&mut self) { self.status = Status::Aborted; }
/// }
///
/// impl Statusable for Worker {
///     fn status(&self) -> Status { self.status }
/// }
///
/// impl Child for Worker {}
///
/// impl RunnableChild for Worker {
///     fn run(&mut self, input: Value) -> ChildResult {
///         self.status = Status::Running;
///         // Do work...
///         let result = json!({"input": input, "processed": true});
///         self.status = Status::Idle;
///         ChildResult::Ok(result)
///     }
/// }
/// ```
pub trait RunnableChild: Child {
    /// Execute work with the given input (synchronous).
    ///
    /// # Arguments
    ///
    /// * `input` - Input data for the work (JSON value)
    ///
    /// # Returns
    ///
    /// - `ChildResult::Ok(value)` on success
    /// - `ChildResult::Err(error)` on failure
    /// - `ChildResult::Aborted` if interrupted by signal
    ///
    /// # Contract
    ///
    /// - Should update status to `Running` at start
    /// - Should update status to `Idle` or `Completed` on success
    /// - Should periodically check for signals during long operations
    fn run(&mut self, input: Value) -> ChildResult;
}

/// Async version of [`RunnableChild`].
///
/// Use this trait for children that perform async I/O operations
/// such as LLM API calls, network requests, or file I/O.
///
/// # Example
///
/// ```ignore
/// use orcs_component::{AsyncRunnableChild, ChildResult, Child};
/// use async_trait::async_trait;
/// use serde_json::Value;
///
/// struct LlmWorker {
///     // ...
/// }
///
/// #[async_trait]
/// impl AsyncRunnableChild for LlmWorker {
///     async fn run(&mut self, input: Value) -> ChildResult {
///         // Async LLM call
///         let response = self.client.complete(&input).await?;
///         ChildResult::Ok(response)
///     }
/// }
/// ```
///
/// # When to Use
///
/// | Trait | Use Case |
/// |-------|----------|
/// | `RunnableChild` | CPU-bound, quick tasks |
/// | `AsyncRunnableChild` | I/O-bound, network, LLM calls |
#[async_trait]
pub trait AsyncRunnableChild: Child {
    /// Execute work with the given input (asynchronous).
    ///
    /// # Arguments
    ///
    /// * `input` - Input data for the work (JSON value)
    ///
    /// # Returns
    ///
    /// - `ChildResult::Ok(value)` on success
    /// - `ChildResult::Err(error)` on failure
    /// - `ChildResult::Aborted` if interrupted by signal
    ///
    /// # Contract
    ///
    /// - Should update status to `Running` at start
    /// - Should update status to `Idle` or `Completed` on success
    /// - Should check for cancellation using `tokio::select!` or similar
    async fn run(&mut self, input: Value) -> ChildResult;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Status;
    use orcs_event::{Signal, SignalResponse};

    struct TestChild {
        id: String,
        status: Status,
    }

    impl Identifiable for TestChild {
        fn id(&self) -> &str {
            &self.id
        }
    }

    impl SignalReceiver for TestChild {
        fn on_signal(&mut self, _signal: &Signal) -> SignalResponse {
            SignalResponse::Ignored
        }

        fn abort(&mut self) {
            self.status = Status::Aborted;
        }
    }

    impl Statusable for TestChild {
        fn status(&self) -> Status {
            self.status
        }
    }

    impl Child for TestChild {}

    #[test]
    fn child_object_safety() {
        let child: Box<dyn Child> = Box::new(TestChild {
            id: "test".into(),
            status: Status::Idle,
        });

        assert_eq!(child.id(), "test");
        assert_eq!(child.status(), Status::Idle);
    }

    #[test]
    fn child_collection() {
        let children: Vec<Box<dyn Child>> = vec![
            Box::new(TestChild {
                id: "child-1".into(),
                status: Status::Running,
            }),
            Box::new(TestChild {
                id: "child-2".into(),
                status: Status::Idle,
            }),
        ];

        assert_eq!(children.len(), 2);
        assert_eq!(children[0].id(), "child-1");
        assert_eq!(children[1].id(), "child-2");
    }

    // --- ChildError tests ---

    #[test]
    fn child_error_kind() {
        assert_eq!(
            ChildError::ExecutionFailed { reason: "x".into() }.kind(),
            "execution_failed"
        );
        assert_eq!(ChildError::InvalidInput("x".into()).kind(), "invalid_input");
        assert_eq!(ChildError::Timeout { elapsed_ms: 100 }.kind(), "timeout");
        assert_eq!(ChildError::Internal("x".into()).kind(), "internal");
    }

    #[test]
    fn child_error_display() {
        let err = ChildError::Timeout { elapsed_ms: 5000 };
        assert_eq!(err.to_string(), "timeout after 5000ms");
    }

    // --- ChildResult tests ---

    #[test]
    fn child_result_ok() {
        let result = ChildResult::Ok(serde_json::json!({"done": true}));
        assert!(result.is_ok());
        assert!(!result.is_err());
        assert!(!result.is_aborted());
    }

    #[test]
    fn child_result_err() {
        let result = ChildResult::Err(ChildError::ExecutionFailed {
            reason: "failed".into(),
        });
        assert!(!result.is_ok());
        assert!(result.is_err());
        assert!(!result.is_aborted());
    }

    #[test]
    fn child_result_aborted() {
        let result = ChildResult::Aborted;
        assert!(!result.is_ok());
        assert!(!result.is_err());
        assert!(result.is_aborted());
    }

    #[test]
    fn child_result_ok_extract() {
        let result = ChildResult::Ok(serde_json::json!(42));
        let value = result.ok();
        assert_eq!(value, Some(serde_json::json!(42)));
    }

    #[test]
    fn child_result_err_extract() {
        let result = ChildResult::Err(ChildError::InvalidInput("bad input".into()));
        let err = result.err();
        assert!(err.is_some());
        assert_eq!(err.unwrap().kind(), "invalid_input");
    }

    #[test]
    fn child_result_default() {
        let result = ChildResult::default();
        assert!(result.is_ok());
        assert_eq!(result.ok(), Some(Value::Null));
    }

    #[test]
    fn child_result_from_value() {
        let result: ChildResult = serde_json::json!({"key": "value"}).into();
        assert!(result.is_ok());
    }

    #[test]
    fn child_result_from_error() {
        let result: ChildResult = ChildError::Internal("oops".into()).into();
        assert!(result.is_err());
    }

    // --- ChildResultDto tests ---

    #[test]
    fn child_result_dto_from_ok() {
        let result = ChildResult::Ok(serde_json::json!({"done": true}));
        let dto: ChildResultDto = result.into();

        assert!(dto.is_ok());
        assert!(!dto.is_err());
        assert!(!dto.is_aborted());
    }

    #[test]
    fn child_result_dto_from_err() {
        let result = ChildResult::Err(ChildError::Timeout { elapsed_ms: 3000 });
        let dto: ChildResultDto = result.into();

        assert!(dto.is_err());
        if let ChildResultDto::Err { kind, message } = dto {
            assert_eq!(kind, "timeout");
            assert!(message.contains("3000"));
        } else {
            panic!("expected Err variant");
        }
    }

    #[test]
    fn child_result_dto_from_aborted() {
        let result = ChildResult::Aborted;
        let dto: ChildResultDto = result.into();

        assert!(dto.is_aborted());
    }

    #[test]
    fn child_result_dto_serialization_err() {
        let dto = ChildResultDto::Err {
            kind: "timeout".into(),
            message: "timeout after 5000ms".into(),
        };

        let json = serde_json::to_string(&dto).unwrap();
        let restored: ChildResultDto = serde_json::from_str(&json).unwrap();

        assert_eq!(dto, restored);
    }

    #[test]
    fn child_result_dto_serialization_ok_roundtrip() {
        let original = ChildResultDto::Ok(serde_json::json!({"key": "value", "count": 42}));
        let json = serde_json::to_string(&original).unwrap();
        let restored: ChildResultDto = serde_json::from_str(&json).unwrap();

        assert_eq!(original, restored);
    }

    #[test]
    fn child_result_dto_serialization_aborted_roundtrip() {
        let original = ChildResultDto::Aborted;
        let json = serde_json::to_string(&original).unwrap();
        let restored: ChildResultDto = serde_json::from_str(&json).unwrap();

        assert_eq!(original, restored);
    }

    #[test]
    fn child_result_to_dto_roundtrip() {
        // ChildResult → ChildResultDto → JSON → ChildResultDto
        let result = ChildResult::Err(ChildError::ExecutionFailed {
            reason: "network error".into(),
        });
        let dto: ChildResultDto = result.into();
        let json = serde_json::to_string(&dto).unwrap();
        let restored: ChildResultDto = serde_json::from_str(&json).unwrap();

        assert_eq!(dto, restored);
        if let ChildResultDto::Err { kind, message } = restored {
            assert_eq!(kind, "execution_failed");
            assert!(message.contains("network error"));
        }
    }

    #[test]
    fn child_result_dto_default() {
        let dto = ChildResultDto::default();
        assert!(dto.is_ok());
    }

    // --- RunnableChild tests ---

    struct TestRunnableChild {
        id: String,
        status: Status,
    }

    impl Identifiable for TestRunnableChild {
        fn id(&self) -> &str {
            &self.id
        }
    }

    impl SignalReceiver for TestRunnableChild {
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

    impl Statusable for TestRunnableChild {
        fn status(&self) -> Status {
            self.status
        }
    }

    impl Child for TestRunnableChild {}

    impl RunnableChild for TestRunnableChild {
        fn run(&mut self, input: Value) -> ChildResult {
            self.status = Status::Running;
            // Simulate work
            let result = serde_json::json!({
                "input": input,
                "processed": true
            });
            self.status = Status::Idle;
            ChildResult::Ok(result)
        }
    }

    #[test]
    fn runnable_child_run() {
        let mut child = TestRunnableChild {
            id: "worker-1".into(),
            status: Status::Idle,
        };

        let input = serde_json::json!({"task": "test"});
        let result = child.run(input.clone());

        assert!(result.is_ok());
        if let ChildResult::Ok(value) = result {
            assert_eq!(value["input"], input);
            assert_eq!(value["processed"], true);
        }
        assert_eq!(child.status(), Status::Idle);
    }

    #[test]
    fn runnable_child_object_safety() {
        let child: Box<dyn RunnableChild> = Box::new(TestRunnableChild {
            id: "worker".into(),
            status: Status::Idle,
        });

        // Can be used as dyn RunnableChild
        assert_eq!(child.id(), "worker");
        assert_eq!(child.status(), Status::Idle);
    }

    // --- AsyncRunnableChild tests ---

    struct TestAsyncChild {
        id: String,
        status: Status,
    }

    impl Identifiable for TestAsyncChild {
        fn id(&self) -> &str {
            &self.id
        }
    }

    impl SignalReceiver for TestAsyncChild {
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

    impl Statusable for TestAsyncChild {
        fn status(&self) -> Status {
            self.status
        }
    }

    impl Child for TestAsyncChild {}

    #[async_trait]
    impl AsyncRunnableChild for TestAsyncChild {
        async fn run(&mut self, input: Value) -> ChildResult {
            self.status = Status::Running;
            // Simulate async work
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            let result = serde_json::json!({
                "input": input,
                "async": true
            });
            self.status = Status::Idle;
            ChildResult::Ok(result)
        }
    }

    #[tokio::test]
    async fn async_runnable_child_run() {
        let mut child = TestAsyncChild {
            id: "async-worker-1".into(),
            status: Status::Idle,
        };

        let input = serde_json::json!({"task": "async_test"});
        let result = child.run(input.clone()).await;

        assert!(result.is_ok());
        if let ChildResult::Ok(value) = result {
            assert_eq!(value["input"], input);
            assert_eq!(value["async"], true);
        }
        assert_eq!(child.status(), Status::Idle);
    }

    #[tokio::test]
    async fn async_runnable_child_object_safety() {
        let child: Box<dyn AsyncRunnableChild> = Box::new(TestAsyncChild {
            id: "async-worker".into(),
            status: Status::Idle,
        });

        // Can be used as dyn AsyncRunnableChild
        assert_eq!(child.id(), "async-worker");
        assert_eq!(child.status(), Status::Idle);
    }
}

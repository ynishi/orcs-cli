//! Testing harnesses for Component and Child implementations.
//!
//! Provides test harnesses for testing Component and Child implementations
//! without requiring the full OrcsEngine infrastructure.
//!
//! # Features
//!
//! - Engine-independent component/child testing
//! - Request/Signal/Lifecycle processing
//! - Event logging for snapshot testing
//! - Deterministic synchronous execution
//! - Async child support with timeout
//! - Automatic time measurement
//!
//! # Component Testing Example
//!
//! ```
//! use orcs_component::testing::{ComponentTestHarness, RequestRecord};
//! use orcs_component::{Component, ComponentError, Status, EventCategory};
//! use orcs_event::{Request, Signal, SignalResponse};
//! use orcs_types::ComponentId;
//! use serde_json::{json, Value};
//!
//! struct EchoComponent {
//!     id: ComponentId,
//!     status: Status,
//! }
//!
//! impl Component for EchoComponent {
//!     fn id(&self) -> &ComponentId { &self.id }
//!     fn status(&self) -> Status { self.status }
//!     fn on_request(&mut self, req: &Request) -> Result<Value, ComponentError> {
//!         Ok(req.payload.clone())
//!     }
//!     fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
//!         if signal.is_veto() {
//!             self.abort();
//!             SignalResponse::Abort
//!         } else {
//!             SignalResponse::Handled
//!         }
//!     }
//!     fn abort(&mut self) { self.status = Status::Aborted; }
//! }
//!
//! let echo = EchoComponent {
//!     id: ComponentId::builtin("echo"),
//!     status: Status::Idle,
//! };
//! let mut harness = ComponentTestHarness::new(echo);
//!
//! // Test request handling
//! let result = harness.request(EventCategory::Echo, "echo", json!({"msg": "hello"}));
//! assert!(result.is_ok());
//!
//! // Test signal handling
//! let response = harness.veto();
//! assert_eq!(response, SignalResponse::Abort);
//! ```
//!
//! # Child Testing Example
//!
//! ```
//! use orcs_component::testing::SyncChildTestHarness;
//! use orcs_component::{Child, RunnableChild, ChildResult, Identifiable, SignalReceiver, Statusable, Status};
//! use orcs_event::{Signal, SignalResponse};
//! use serde_json::{json, Value};
//!
//! struct Worker {
//!     id: String,
//!     status: Status,
//! }
//!
//! impl Identifiable for Worker {
//!     fn id(&self) -> &str { &self.id }
//! }
//!
//! impl SignalReceiver for Worker {
//!     fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
//!         if signal.is_veto() {
//!             self.abort();
//!             SignalResponse::Abort
//!         } else {
//!             SignalResponse::Handled
//!         }
//!     }
//!     fn abort(&mut self) { self.status = Status::Aborted; }
//! }
//!
//! impl Statusable for Worker {
//!     fn status(&self) -> Status { self.status }
//! }
//!
//! impl Child for Worker {}
//!
//! impl RunnableChild for Worker {
//!     fn run(&mut self, input: Value) -> ChildResult {
//!         self.status = Status::Running;
//!         let result = json!({"processed": input});
//!         self.status = Status::Idle;
//!         ChildResult::Ok(result)
//!     }
//! }
//!
//! let worker = Worker { id: "worker-1".into(), status: Status::Idle };
//! let mut harness = SyncChildTestHarness::new(worker);
//!
//! // Test run
//! let result = harness.run(json!({"task": "test"}));
//! assert!(result.is_ok());
//!
//! // Test signal handling
//! let response = harness.veto();
//! assert_eq!(response, SignalResponse::Abort);
//!
//! // Check logs
//! assert_eq!(harness.run_log().len(), 1);
//! assert!(harness.run_log()[0].elapsed_ms.is_some());
//! ```

use crate::{
    AsyncRunnableChild, Child, ChildResult, Component, ComponentError, EventCategory,
    RunnableChild, Status,
};
use orcs_event::{Signal, SignalResponse};
use orcs_types::{ChannelId, ComponentId, Principal};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{Duration, Instant};
use thiserror::Error;

/// Record of a request sent to a component.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    /// Operation name.
    pub operation: String,
    /// Event category.
    pub category: String,
    /// Result of the request.
    pub result: RequestResult,
}

/// Result of a request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum RequestResult {
    /// Request succeeded with a value.
    Ok(Value),
    /// Request failed with an error message.
    Err(String),
}

/// Record of a signal sent to a component.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalRecord {
    /// Signal kind (e.g., "Veto", "Cancel", "Approve").
    pub kind: String,
    /// Response from the component.
    pub response: String,
}

/// Test harness for Component implementations.
///
/// Provides a minimal testing environment for components
/// without requiring OrcsEngine or EventBus.
pub struct ComponentTestHarness<C: Component> {
    /// The component under test.
    component: C,
    /// Log of requests sent to the component.
    request_log: Vec<RequestRecord>,
    /// Log of signals sent to the component.
    signal_log: Vec<SignalRecord>,
    /// Test channel ID for request context.
    test_channel: ChannelId,
}

impl<C: Component> ComponentTestHarness<C> {
    /// Creates a new test harness for the given component.
    pub fn new(component: C) -> Self {
        Self {
            component,
            request_log: Vec::new(),
            signal_log: Vec::new(),
            test_channel: ChannelId::new(),
        }
    }

    /// Returns a reference to the component under test.
    pub fn component(&self) -> &C {
        &self.component
    }

    /// Returns a mutable reference to the component under test.
    pub fn component_mut(&mut self) -> &mut C {
        &mut self.component
    }

    /// Calls `init()` on the component with an empty config.
    ///
    /// # Errors
    ///
    /// Returns the component's initialization error if any.
    pub fn init(&mut self) -> Result<(), ComponentError> {
        self.component
            .init(&serde_json::Value::Object(serde_json::Map::new()))
    }

    /// Calls `init()` on the component with the given config.
    ///
    /// # Errors
    ///
    /// Returns the component's initialization error if any.
    pub fn init_with_config(&mut self, config: &serde_json::Value) -> Result<(), ComponentError> {
        self.component.init(config)
    }

    /// Sends a request to the component and logs the result.
    ///
    /// # Arguments
    ///
    /// * `request` - The request to send
    ///
    /// # Returns
    ///
    /// The result of the request.
    pub fn send_request(&mut self, request: &orcs_event::Request) -> Result<Value, ComponentError> {
        let result = self.component.on_request(request);

        self.request_log.push(RequestRecord {
            operation: request.operation.clone(),
            category: request.category.name(),
            result: match &result {
                Ok(v) => RequestResult::Ok(v.clone()),
                Err(e) => RequestResult::Err(e.to_string()),
            },
        });

        result
    }

    /// Sends a request with the given category, operation, and payload.
    ///
    /// Convenience method that builds the request internally.
    ///
    /// # Arguments
    ///
    /// * `category` - Event category
    /// * `operation` - Operation name
    /// * `payload` - Request payload
    ///
    /// # Returns
    ///
    /// The result of the request.
    pub fn request(
        &mut self,
        category: EventCategory,
        operation: &str,
        payload: Value,
    ) -> Result<Value, ComponentError> {
        let req = orcs_event::Request::new(
            category,
            operation,
            self.component.id().clone(),
            self.test_channel,
            payload,
        );
        self.send_request(&req)
    }

    /// Sends a signal to the component and logs the response.
    ///
    /// # Arguments
    ///
    /// * `signal` - The signal to send
    ///
    /// # Returns
    ///
    /// The component's response to the signal.
    pub fn send_signal(&mut self, signal: Signal) -> SignalResponse {
        let response = self.component.on_signal(&signal);

        self.signal_log.push(SignalRecord {
            kind: format!("{:?}", signal.kind),
            response: format!("{:?}", response),
        });

        response
    }

    /// Sends a Veto signal to the component.
    ///
    /// # Returns
    ///
    /// The component's response (should be `Abort`).
    pub fn veto(&mut self) -> SignalResponse {
        self.send_signal(Signal::veto(Principal::System))
    }

    /// Sends a Cancel signal for the test channel.
    ///
    /// # Returns
    ///
    /// The component's response.
    pub fn cancel(&mut self) -> SignalResponse {
        self.send_signal(Signal::cancel(self.test_channel, Principal::System))
    }

    /// Sends a Cancel signal for a specific channel.
    ///
    /// # Arguments
    ///
    /// * `channel` - The channel to cancel
    ///
    /// # Returns
    ///
    /// The component's response.
    pub fn cancel_channel(&mut self, channel: ChannelId) -> SignalResponse {
        self.send_signal(Signal::cancel(channel, Principal::System))
    }

    /// Sends an Approve signal.
    ///
    /// # Arguments
    ///
    /// * `approval_id` - The approval request ID
    ///
    /// # Returns
    ///
    /// The component's response.
    pub fn approve(&mut self, approval_id: &str) -> SignalResponse {
        self.send_signal(Signal::approve(approval_id, Principal::System))
    }

    /// Sends a Reject signal.
    ///
    /// # Arguments
    ///
    /// * `approval_id` - The approval request ID
    /// * `reason` - Optional rejection reason
    ///
    /// # Returns
    ///
    /// The component's response.
    pub fn reject(&mut self, approval_id: &str, reason: Option<String>) -> SignalResponse {
        self.send_signal(Signal::reject(approval_id, reason, Principal::System))
    }

    /// Calls `abort()` on the component.
    pub fn abort(&mut self) {
        self.component.abort();
    }

    /// Calls `shutdown()` on the component.
    pub fn shutdown(&mut self) {
        self.component.shutdown();
    }

    /// Returns the current status of the component.
    pub fn status(&self) -> Status {
        self.component.status()
    }

    /// Returns the component ID.
    pub fn id(&self) -> &ComponentId {
        self.component.id()
    }

    /// Returns the request log for snapshot testing.
    pub fn request_log(&self) -> &[RequestRecord] {
        &self.request_log
    }

    /// Returns the signal log for snapshot testing.
    pub fn signal_log(&self) -> &[SignalRecord] {
        &self.signal_log
    }

    /// Clears all logs.
    pub fn clear_logs(&mut self) {
        self.request_log.clear();
        self.signal_log.clear();
    }

    /// Returns the test channel ID.
    pub fn test_channel(&self) -> ChannelId {
        self.test_channel
    }

    /// Sets a custom test channel ID.
    pub fn with_channel(mut self, channel: ChannelId) -> Self {
        self.test_channel = channel;
        self
    }
}

// =============================================================================
// Child Test Harness Types
// =============================================================================

/// Record of a run() call on a child.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    /// Input value passed to run().
    pub input: Value,
    /// Result of the run() call.
    pub result: RunResult,
    /// Elapsed time in milliseconds.
    pub elapsed_ms: Option<u64>,
}

/// Result of a run() call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RunResult {
    /// Run completed successfully.
    Ok {
        /// Output value.
        value: Value,
    },
    /// Run failed with an error.
    Err {
        /// Error kind identifier.
        kind: String,
        /// Error message.
        message: String,
    },
    /// Run was aborted.
    Aborted,
}

impl From<&ChildResult> for RunResult {
    fn from(result: &ChildResult) -> Self {
        match result {
            ChildResult::Ok(v) => Self::Ok { value: v.clone() },
            ChildResult::Err(e) => Self::Err {
                kind: e.kind().to_string(),
                message: e.to_string(),
            },
            ChildResult::Aborted => Self::Aborted,
        }
    }
}

/// Error returned when an async operation times out.
#[derive(Debug, Clone, Error)]
#[error("operation timed out after {elapsed_ms}ms (limit: {timeout_ms}ms)")]
pub struct TimeoutError {
    /// Elapsed time before timeout.
    pub elapsed_ms: u64,
    /// Configured timeout.
    pub timeout_ms: u64,
}

// =============================================================================
// ChildTestHarness (Base - for Child trait)
// =============================================================================

/// Test harness for Child trait implementations.
///
/// Provides signal handling and status testing for any Child.
/// For testing `run()`, use [`SyncChildTestHarness`] or [`AsyncChildTestHarness`].
///
/// # Example
///
/// ```
/// use orcs_component::testing::ChildTestHarness;
/// use orcs_component::{Child, Identifiable, SignalReceiver, Statusable, Status};
/// use orcs_event::{Signal, SignalResponse};
///
/// struct PassiveChild {
///     id: String,
///     status: Status,
/// }
///
/// impl Identifiable for PassiveChild {
///     fn id(&self) -> &str { &self.id }
/// }
///
/// impl SignalReceiver for PassiveChild {
///     fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
///         if signal.is_veto() {
///             self.abort();
///             SignalResponse::Abort
///         } else {
///             SignalResponse::Handled
///         }
///     }
///     fn abort(&mut self) { self.status = Status::Aborted; }
/// }
///
/// impl Statusable for PassiveChild {
///     fn status(&self) -> Status { self.status }
/// }
///
/// impl Child for PassiveChild {}
///
/// let child = PassiveChild { id: "test".into(), status: Status::Idle };
/// let mut harness = ChildTestHarness::new(child);
///
/// assert_eq!(harness.status(), Status::Idle);
/// harness.veto();
/// assert_eq!(harness.status(), Status::Aborted);
/// ```
pub struct ChildTestHarness<C: Child> {
    /// The child under test.
    child: C,
    /// Log of signals sent to the child.
    signal_log: Vec<SignalRecord>,
}

impl<C: Child> ChildTestHarness<C> {
    /// Creates a new test harness for the given child.
    pub fn new(child: C) -> Self {
        Self {
            child,
            signal_log: Vec::new(),
        }
    }

    /// Returns a reference to the child under test.
    pub fn child(&self) -> &C {
        &self.child
    }

    /// Returns a mutable reference to the child under test.
    pub fn child_mut(&mut self) -> &mut C {
        &mut self.child
    }

    /// Returns the child's ID.
    pub fn id(&self) -> &str {
        self.child.id()
    }

    /// Returns the current status of the child.
    pub fn status(&self) -> Status {
        self.child.status()
    }

    /// Sends a signal to the child and logs the response.
    pub fn send_signal(&mut self, signal: Signal) -> SignalResponse {
        let response = self.child.on_signal(&signal);

        self.signal_log.push(SignalRecord {
            kind: format!("{:?}", signal.kind),
            response: format!("{:?}", response),
        });

        response
    }

    /// Sends a Veto signal to the child.
    pub fn veto(&mut self) -> SignalResponse {
        self.send_signal(Signal::veto(Principal::System))
    }

    /// Sends a Cancel signal to the child.
    pub fn cancel(&mut self) -> SignalResponse {
        self.send_signal(Signal::cancel(ChannelId::new(), Principal::System))
    }

    /// Calls `abort()` on the child.
    pub fn abort(&mut self) {
        self.child.abort();
    }

    /// Returns the signal log for snapshot testing.
    pub fn signal_log(&self) -> &[SignalRecord] {
        &self.signal_log
    }

    /// Clears all logs.
    pub fn clear_logs(&mut self) {
        self.signal_log.clear();
    }
}

// =============================================================================
// SyncChildTestHarness (for RunnableChild)
// =============================================================================

/// Test harness for synchronous RunnableChild implementations.
///
/// Provides run() testing with automatic time measurement.
///
/// # Example
///
/// ```
/// use orcs_component::testing::SyncChildTestHarness;
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
///             self.abort();
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
///         ChildResult::Ok(json!({"echo": input}))
///     }
/// }
///
/// let worker = Worker { id: "worker".into(), status: Status::Idle };
/// let mut harness = SyncChildTestHarness::new(worker);
///
/// let result = harness.run(json!({"task": "test"}));
/// assert!(result.is_ok());
/// assert_eq!(harness.run_log().len(), 1);
/// ```
pub struct SyncChildTestHarness<C: RunnableChild> {
    /// Inner base harness.
    inner: ChildTestHarness<C>,
    /// Log of run() calls.
    run_log: Vec<RunRecord>,
    /// Whether to measure execution time.
    measure_time: bool,
}

impl<C: RunnableChild> SyncChildTestHarness<C> {
    /// Creates a new test harness for the given runnable child.
    ///
    /// Time measurement is enabled by default.
    pub fn new(child: C) -> Self {
        Self {
            inner: ChildTestHarness::new(child),
            run_log: Vec::new(),
            measure_time: true,
        }
    }

    /// Disables time measurement.
    pub fn without_time_measurement(mut self) -> Self {
        self.measure_time = false;
        self
    }

    /// Executes run() on the child with the given input.
    pub fn run(&mut self, input: Value) -> ChildResult {
        let start = if self.measure_time {
            Some(Instant::now())
        } else {
            None
        };

        let result = self.inner.child.run(input.clone());

        let elapsed_ms = start.map(|s| s.elapsed().as_millis() as u64);

        self.run_log.push(RunRecord {
            input,
            result: RunResult::from(&result),
            elapsed_ms,
        });

        result
    }

    /// Executes run() with a serializable input.
    pub fn run_json<T: Serialize>(&mut self, input: T) -> ChildResult {
        let value = serde_json::to_value(input).unwrap_or(Value::Null);
        self.run(value)
    }

    /// Returns the run log for snapshot testing.
    pub fn run_log(&self) -> &[RunRecord] {
        &self.run_log
    }

    /// Clears all logs (both run and signal).
    pub fn clear_all_logs(&mut self) {
        self.run_log.clear();
        self.inner.clear_logs();
    }

    // --- Delegate to inner ChildTestHarness ---

    /// Returns a reference to the child under test.
    pub fn child(&self) -> &C {
        self.inner.child()
    }

    /// Returns a mutable reference to the child under test.
    pub fn child_mut(&mut self) -> &mut C {
        self.inner.child_mut()
    }

    /// Returns the child's ID.
    pub fn id(&self) -> &str {
        self.inner.id()
    }

    /// Returns the current status of the child.
    pub fn status(&self) -> Status {
        self.inner.status()
    }

    /// Sends a signal to the child.
    pub fn send_signal(&mut self, signal: Signal) -> SignalResponse {
        self.inner.send_signal(signal)
    }

    /// Sends a Veto signal to the child.
    pub fn veto(&mut self) -> SignalResponse {
        self.inner.veto()
    }

    /// Sends a Cancel signal to the child.
    pub fn cancel(&mut self) -> SignalResponse {
        self.inner.cancel()
    }

    /// Calls `abort()` on the child.
    pub fn abort(&mut self) {
        self.inner.abort();
    }

    /// Returns the signal log.
    pub fn signal_log(&self) -> &[SignalRecord] {
        self.inner.signal_log()
    }
}

// =============================================================================
// AsyncChildTestHarness (for AsyncRunnableChild)
// =============================================================================

/// Test harness for asynchronous AsyncRunnableChild implementations.
///
/// Provides async run() testing with automatic time measurement and timeout support.
///
/// # Example
///
/// ```ignore
/// use orcs_component::testing::AsyncChildTestHarness;
/// use orcs_component::{Child, AsyncRunnableChild, ChildResult, async_trait};
/// use serde_json::{json, Value};
/// use std::time::Duration;
///
/// struct AsyncWorker { /* ... */ }
///
/// #[async_trait]
/// impl AsyncRunnableChild for AsyncWorker {
///     async fn run(&mut self, input: Value) -> ChildResult {
///         tokio::time::sleep(Duration::from_millis(10)).await;
///         ChildResult::Ok(json!({"processed": input}))
///     }
/// }
///
/// #[tokio::test]
/// async fn test_async_worker() {
///     let worker = AsyncWorker::new();
///     let mut harness = AsyncChildTestHarness::new(worker)
///         .with_default_timeout(Duration::from_secs(5));
///
///     let result = harness.run(json!({"task": "test"})).await;
///     assert!(result.is_ok());
///
///     // Test timeout
///     let slow_result = harness
///         .run_with_timeout(json!({}), Duration::from_millis(1))
///         .await;
///     assert!(slow_result.is_err());
/// }
/// ```
pub struct AsyncChildTestHarness<C: AsyncRunnableChild> {
    /// Inner base harness.
    inner: ChildTestHarness<C>,
    /// Log of run() calls.
    run_log: Vec<RunRecord>,
    /// Whether to measure execution time.
    measure_time: bool,
    /// Default timeout for run operations.
    default_timeout: Option<Duration>,
}

impl<C: AsyncRunnableChild> AsyncChildTestHarness<C> {
    /// Creates a new test harness for the given async runnable child.
    ///
    /// Time measurement is enabled by default.
    pub fn new(child: C) -> Self {
        Self {
            inner: ChildTestHarness::new(child),
            run_log: Vec::new(),
            measure_time: true,
            default_timeout: None,
        }
    }

    /// Disables time measurement.
    pub fn without_time_measurement(mut self) -> Self {
        self.measure_time = false;
        self
    }

    /// Sets the default timeout for run operations.
    pub fn with_default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = Some(timeout);
        self
    }

    /// Executes async run() on the child with the given input.
    ///
    /// If a default timeout is set, it will be applied.
    pub async fn run(&mut self, input: Value) -> ChildResult {
        if let Some(timeout) = self.default_timeout {
            match self.run_with_timeout(input, timeout).await {
                Ok(result) => result,
                Err(_) => ChildResult::Aborted,
            }
        } else {
            self.run_inner(input).await
        }
    }

    /// Executes async run() with a timeout.
    ///
    /// Returns `Err(TimeoutError)` if the operation times out.
    pub async fn run_with_timeout(
        &mut self,
        input: Value,
        timeout: Duration,
    ) -> Result<ChildResult, TimeoutError> {
        let start = Instant::now();

        match tokio::time::timeout(timeout, self.run_inner(input)).await {
            Ok(result) => Ok(result),
            Err(_) => Err(TimeoutError {
                elapsed_ms: start.elapsed().as_millis() as u64,
                timeout_ms: timeout.as_millis() as u64,
            }),
        }
    }

    /// Internal run implementation.
    async fn run_inner(&mut self, input: Value) -> ChildResult {
        let start = if self.measure_time {
            Some(Instant::now())
        } else {
            None
        };

        let result = self.inner.child.run(input.clone()).await;

        let elapsed_ms = start.map(|s| s.elapsed().as_millis() as u64);

        self.run_log.push(RunRecord {
            input,
            result: RunResult::from(&result),
            elapsed_ms,
        });

        result
    }

    /// Executes async run() with a serializable input.
    pub async fn run_json<T: Serialize>(&mut self, input: T) -> ChildResult {
        let value = serde_json::to_value(input).unwrap_or(Value::Null);
        self.run(value).await
    }

    /// Returns the run log for snapshot testing.
    pub fn run_log(&self) -> &[RunRecord] {
        &self.run_log
    }

    /// Clears all logs (both run and signal).
    pub fn clear_all_logs(&mut self) {
        self.run_log.clear();
        self.inner.clear_logs();
    }

    // --- Delegate to inner ChildTestHarness ---

    /// Returns a reference to the child under test.
    pub fn child(&self) -> &C {
        self.inner.child()
    }

    /// Returns a mutable reference to the child under test.
    pub fn child_mut(&mut self) -> &mut C {
        self.inner.child_mut()
    }

    /// Returns the child's ID.
    pub fn id(&self) -> &str {
        self.inner.id()
    }

    /// Returns the current status of the child.
    pub fn status(&self) -> Status {
        self.inner.status()
    }

    /// Sends a signal to the child.
    pub fn send_signal(&mut self, signal: Signal) -> SignalResponse {
        self.inner.send_signal(signal)
    }

    /// Sends a Veto signal to the child.
    pub fn veto(&mut self) -> SignalResponse {
        self.inner.veto()
    }

    /// Sends a Cancel signal to the child.
    pub fn cancel(&mut self) -> SignalResponse {
        self.inner.cancel()
    }

    /// Calls `abort()` on the child.
    pub fn abort(&mut self) {
        self.inner.abort();
    }

    /// Returns the signal log.
    pub fn signal_log(&self) -> &[SignalRecord] {
        self.inner.signal_log()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Identifiable, SignalReceiver, Status, Statusable};

    struct TestComponent {
        id: ComponentId,
        status: Status,
        request_count: usize,
    }

    impl TestComponent {
        fn new(name: &str) -> Self {
            Self {
                id: ComponentId::builtin(name),
                status: Status::Idle,
                request_count: 0,
            }
        }
    }

    impl Component for TestComponent {
        fn id(&self) -> &ComponentId {
            &self.id
        }

        fn status(&self) -> Status {
            self.status
        }

        fn on_request(&mut self, request: &orcs_event::Request) -> Result<Value, ComponentError> {
            self.request_count += 1;
            match request.operation.as_str() {
                "echo" => Ok(request.payload.clone()),
                "count" => Ok(Value::Number(self.request_count.into())),
                _ => Err(ComponentError::NotSupported(request.operation.clone())),
            }
        }

        fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
            if signal.is_veto() {
                self.abort();
                SignalResponse::Abort
            } else if matches!(signal.kind, orcs_event::SignalKind::Cancel) {
                SignalResponse::Handled
            } else {
                SignalResponse::Ignored
            }
        }

        fn abort(&mut self) {
            self.status = Status::Aborted;
        }
    }

    #[test]
    fn harness_request() {
        let comp = TestComponent::new("test");
        let mut harness = ComponentTestHarness::new(comp);

        let result = harness.request(
            EventCategory::Echo,
            "echo",
            serde_json::json!({"msg": "hello"}),
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), serde_json::json!({"msg": "hello"}));

        assert_eq!(harness.request_log().len(), 1);
        assert_eq!(harness.request_log()[0].operation, "echo");
    }

    #[test]
    fn harness_request_error() {
        let comp = TestComponent::new("test");
        let mut harness = ComponentTestHarness::new(comp);

        let result = harness.request(EventCategory::Echo, "unknown", Value::Null);
        assert!(result.is_err());

        assert_eq!(harness.request_log().len(), 1);
        assert!(matches!(
            harness.request_log()[0].result,
            RequestResult::Err(_)
        ));
    }

    #[test]
    fn harness_veto() {
        let comp = TestComponent::new("test");
        let mut harness = ComponentTestHarness::new(comp);

        assert_eq!(harness.status(), Status::Idle);

        let response = harness.veto();
        assert_eq!(response, SignalResponse::Abort);
        assert_eq!(harness.status(), Status::Aborted);

        assert_eq!(harness.signal_log().len(), 1);
        assert!(harness.signal_log()[0].kind.contains("Veto"));
    }

    #[test]
    fn harness_cancel() {
        let comp = TestComponent::new("test");
        let mut harness = ComponentTestHarness::new(comp);

        let response = harness.cancel();
        assert_eq!(response, SignalResponse::Handled);
        assert_eq!(harness.status(), Status::Idle);
    }

    #[test]
    fn harness_component_access() {
        let comp = TestComponent::new("test");
        let mut harness = ComponentTestHarness::new(comp);

        assert_eq!(harness.component().request_count, 0);

        harness
            .request(EventCategory::Echo, "count", Value::Null)
            .unwrap();
        assert_eq!(harness.component().request_count, 1);

        harness.component_mut().request_count = 100;
        assert_eq!(harness.component().request_count, 100);
    }

    #[test]
    fn harness_clear_logs() {
        let comp = TestComponent::new("test");
        let mut harness = ComponentTestHarness::new(comp);

        harness
            .request(EventCategory::Echo, "echo", Value::Null)
            .unwrap();
        harness.cancel();

        assert_eq!(harness.request_log().len(), 1);
        assert_eq!(harness.signal_log().len(), 1);

        harness.clear_logs();

        assert_eq!(harness.request_log().len(), 0);
        assert_eq!(harness.signal_log().len(), 0);
    }

    #[test]
    fn harness_init_shutdown() {
        let comp = TestComponent::new("test");
        let mut harness = ComponentTestHarness::new(comp);

        assert!(harness.init().is_ok());
        harness.shutdown();
        // Component should still be accessible after shutdown
        assert_eq!(harness.id().name, "test");
    }

    // =========================================================================
    // Child Test Harness Tests
    // =========================================================================

    use crate::{Child, ChildError, RunnableChild};

    struct TestChild {
        id: String,
        status: Status,
    }

    impl TestChild {
        fn new(id: &str) -> Self {
            Self {
                id: id.into(),
                status: Status::Idle,
            }
        }
    }

    impl Identifiable for TestChild {
        fn id(&self) -> &str {
            &self.id
        }
    }

    impl SignalReceiver for TestChild {
        fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
            if signal.is_veto() {
                self.abort();
                SignalResponse::Abort
            } else {
                SignalResponse::Handled
            }
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

    // --- ChildTestHarness (base) tests ---

    #[test]
    fn child_harness_new() {
        let child = TestChild::new("test-child");
        let harness = ChildTestHarness::new(child);

        assert_eq!(harness.id(), "test-child");
        assert_eq!(harness.status(), Status::Idle);
        assert!(harness.signal_log().is_empty());
    }

    #[test]
    fn child_harness_veto() {
        let child = TestChild::new("test");
        let mut harness = ChildTestHarness::new(child);

        let response = harness.veto();
        assert_eq!(response, SignalResponse::Abort);
        assert_eq!(harness.status(), Status::Aborted);

        assert_eq!(harness.signal_log().len(), 1);
        assert!(harness.signal_log()[0].kind.contains("Veto"));
    }

    #[test]
    fn child_harness_cancel() {
        let child = TestChild::new("test");
        let mut harness = ChildTestHarness::new(child);

        let response = harness.cancel();
        assert_eq!(response, SignalResponse::Handled);
        assert_eq!(harness.status(), Status::Idle);
    }

    #[test]
    fn child_harness_clear_logs() {
        let child = TestChild::new("test");
        let mut harness = ChildTestHarness::new(child);

        harness.veto();
        harness.cancel();
        assert_eq!(harness.signal_log().len(), 2);

        harness.clear_logs();
        assert!(harness.signal_log().is_empty());
    }

    #[test]
    fn child_harness_access() {
        let child = TestChild::new("test");
        let mut harness = ChildTestHarness::new(child);

        assert_eq!(harness.child().id, "test");
        harness.child_mut().status = Status::Running;
        assert_eq!(harness.status(), Status::Running);
    }

    // --- SyncChildTestHarness tests ---

    struct TestRunnableChild {
        id: String,
        status: Status,
        run_count: usize,
    }

    impl TestRunnableChild {
        fn new(id: &str) -> Self {
            Self {
                id: id.into(),
                status: Status::Idle,
                run_count: 0,
            }
        }
    }

    impl Identifiable for TestRunnableChild {
        fn id(&self) -> &str {
            &self.id
        }
    }

    impl SignalReceiver for TestRunnableChild {
        fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
            if signal.is_veto() {
                self.abort();
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
            self.run_count += 1;

            if input.get("fail").is_some() {
                self.status = Status::Idle;
                return ChildResult::Err(ChildError::ExecutionFailed {
                    reason: "requested failure".into(),
                });
            }

            self.status = Status::Idle;
            ChildResult::Ok(serde_json::json!({
                "echo": input,
                "count": self.run_count
            }))
        }
    }

    #[test]
    fn sync_child_harness_run() {
        let child = TestRunnableChild::new("worker");
        let mut harness = SyncChildTestHarness::new(child);

        let result = harness.run(serde_json::json!({"task": "test"}));
        assert!(result.is_ok());

        assert_eq!(harness.run_log().len(), 1);
        assert!(harness.run_log()[0].elapsed_ms.is_some());

        if let RunResult::Ok { value } = &harness.run_log()[0].result {
            assert_eq!(value["count"], 1);
        } else {
            panic!("expected Ok result");
        }
    }

    #[test]
    fn sync_child_harness_run_error() {
        let child = TestRunnableChild::new("worker");
        let mut harness = SyncChildTestHarness::new(child);

        let result = harness.run(serde_json::json!({"fail": true}));
        assert!(result.is_err());

        assert_eq!(harness.run_log().len(), 1);
        assert!(matches!(harness.run_log()[0].result, RunResult::Err { .. }));
    }

    #[test]
    fn sync_child_harness_run_json() {
        #[derive(Serialize)]
        struct Input {
            task: String,
        }

        let child = TestRunnableChild::new("worker");
        let mut harness = SyncChildTestHarness::new(child);

        let result = harness.run_json(Input {
            task: "test".into(),
        });
        assert!(result.is_ok());

        assert_eq!(harness.run_log()[0].input["task"], "test");
    }

    #[test]
    fn sync_child_harness_without_time_measurement() {
        let child = TestRunnableChild::new("worker");
        let mut harness = SyncChildTestHarness::new(child).without_time_measurement();

        harness.run(Value::Null);

        assert!(harness.run_log()[0].elapsed_ms.is_none());
    }

    #[test]
    fn sync_child_harness_veto() {
        let child = TestRunnableChild::new("worker");
        let mut harness = SyncChildTestHarness::new(child);

        let response = harness.veto();
        assert_eq!(response, SignalResponse::Abort);
        assert_eq!(harness.status(), Status::Aborted);
    }

    #[test]
    fn sync_child_harness_clear_all_logs() {
        let child = TestRunnableChild::new("worker");
        let mut harness = SyncChildTestHarness::new(child);

        harness.run(Value::Null);
        harness.veto();

        assert_eq!(harness.run_log().len(), 1);
        assert_eq!(harness.signal_log().len(), 1);

        harness.clear_all_logs();

        assert!(harness.run_log().is_empty());
        assert!(harness.signal_log().is_empty());
    }

    #[test]
    fn sync_child_harness_multiple_runs() {
        let child = TestRunnableChild::new("worker");
        let mut harness = SyncChildTestHarness::new(child);

        for i in 0..5 {
            harness.run(serde_json::json!({"iteration": i}));
        }

        assert_eq!(harness.run_log().len(), 5);
        assert_eq!(harness.child().run_count, 5);
    }

    // --- AsyncChildTestHarness tests ---

    use crate::AsyncRunnableChild;
    use async_trait::async_trait;

    struct TestAsyncChild {
        id: String,
        status: Status,
        delay_ms: u64,
    }

    impl TestAsyncChild {
        fn new(id: &str) -> Self {
            Self {
                id: id.into(),
                status: Status::Idle,
                delay_ms: 0,
            }
        }

        fn with_delay(mut self, delay_ms: u64) -> Self {
            self.delay_ms = delay_ms;
            self
        }
    }

    impl Identifiable for TestAsyncChild {
        fn id(&self) -> &str {
            &self.id
        }
    }

    impl SignalReceiver for TestAsyncChild {
        fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
            if signal.is_veto() {
                self.abort();
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

            if self.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            }

            self.status = Status::Idle;
            ChildResult::Ok(serde_json::json!({
                "async": true,
                "input": input
            }))
        }
    }

    #[tokio::test]
    async fn async_child_harness_run() {
        let child = TestAsyncChild::new("async-worker");
        let mut harness = AsyncChildTestHarness::new(child);

        let result = harness.run(serde_json::json!({"task": "async_test"})).await;
        assert!(result.is_ok());

        assert_eq!(harness.run_log().len(), 1);
        assert!(harness.run_log()[0].elapsed_ms.is_some());

        if let RunResult::Ok { value } = &harness.run_log()[0].result {
            assert_eq!(value["async"], true);
        }
    }

    #[tokio::test]
    async fn async_child_harness_with_timeout_success() {
        let child = TestAsyncChild::new("async-worker").with_delay(10);
        let mut harness = AsyncChildTestHarness::new(child);

        let result = harness
            .run_with_timeout(Value::Null, Duration::from_millis(100))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn async_child_harness_with_timeout_failure() {
        let child = TestAsyncChild::new("async-worker").with_delay(100);
        let mut harness = AsyncChildTestHarness::new(child);

        let result = harness
            .run_with_timeout(Value::Null, Duration::from_millis(10))
            .await;
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert!(err.timeout_ms == 10);
    }

    #[tokio::test]
    async fn async_child_harness_with_default_timeout() {
        let child = TestAsyncChild::new("async-worker").with_delay(100);
        let mut harness =
            AsyncChildTestHarness::new(child).with_default_timeout(Duration::from_millis(10));

        // Should timeout and return Aborted
        let result = harness.run(Value::Null).await;
        assert!(result.is_aborted());
    }

    #[tokio::test]
    async fn async_child_harness_veto() {
        let child = TestAsyncChild::new("async-worker");
        let mut harness = AsyncChildTestHarness::new(child);

        let response = harness.veto();
        assert_eq!(response, SignalResponse::Abort);
        assert_eq!(harness.status(), Status::Aborted);
    }

    #[tokio::test]
    async fn async_child_harness_clear_all_logs() {
        let child = TestAsyncChild::new("async-worker");
        let mut harness = AsyncChildTestHarness::new(child);

        harness.run(Value::Null).await;
        harness.veto();

        assert_eq!(harness.run_log().len(), 1);
        assert_eq!(harness.signal_log().len(), 1);

        harness.clear_all_logs();

        assert!(harness.run_log().is_empty());
        assert!(harness.signal_log().is_empty());
    }

    // --- RunResult serialization tests ---

    #[test]
    fn run_result_serialization() {
        let ok_result = RunResult::Ok {
            value: serde_json::json!({"key": "value"}),
        };
        let json = serde_json::to_string(&ok_result).unwrap();
        assert!(json.contains("\"type\":\"Ok\""));

        let err_result = RunResult::Err {
            kind: "timeout".into(),
            message: "timed out".into(),
        };
        let json = serde_json::to_string(&err_result).unwrap();
        assert!(json.contains("\"type\":\"Err\""));

        let aborted_result = RunResult::Aborted;
        let json = serde_json::to_string(&aborted_result).unwrap();
        assert!(json.contains("\"type\":\"Aborted\""));
    }

    #[test]
    fn run_record_serialization() {
        let record = RunRecord {
            input: serde_json::json!({"task": "test"}),
            result: RunResult::Ok {
                value: serde_json::json!({"done": true}),
            },
            elapsed_ms: Some(42),
        };

        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"elapsed_ms\":42"));

        let restored: RunRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.elapsed_ms, Some(42));
    }
}

//! Child spawner for managing spawned children within a Runner.
//!
//! The ChildSpawner manages the lifecycle of children spawned by
//! a Component or Child. It handles:
//!
//! - Spawning new children from configs
//! - Signal propagation to all children
//! - Tracking child status
//! - Cleanup on abort
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                     ChildSpawner                             │
//! │                                                              │
//! │  spawn(config) ──► ManagedChild ──► RunnableChild           │
//! │                         │                                    │
//! │                         └─► child.status() (direct query)   │
//! │                                                              │
//! │  propagate_signal() ──► all children (via on_signal)        │
//! │  abort_all() ──► stop all children                          │
//! │  reap_finished() ──► remove completed/aborted children      │
//! └─────────────────────────────────────────────────────────────┘
//! ```

use orcs_component::{
    async_trait, AsyncChildHandle, ChildConfig, ChildHandle, ChildResult, RunError, RunnableChild,
    SpawnError, Status,
};
use orcs_event::Signal;
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Default maximum number of children per spawner.
const DEFAULT_MAX_CHILDREN: usize = 64;

/// A managed child instance.
struct ManagedChild {
    /// The child instance.
    child: Arc<Mutex<Box<dyn RunnableChild>>>,
}

/// Handle to a spawned child.
///
/// Provides control over a child that was spawned via [`ChildSpawner::spawn()`].
pub struct SpawnedChildHandle {
    /// Child ID.
    id: String,
    /// The child instance (shared with ManagedChild).
    child: Arc<Mutex<Box<dyn RunnableChild>>>,
}

impl std::fmt::Debug for SpawnedChildHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("SpawnedChildHandle");
        s.field("id", &self.id);

        // Use try_lock to avoid deadlock when Debug is called while holding lock
        match self.child.try_lock() {
            Ok(guard) => s.field("status", &guard.status()),
            Err(_) => s.field("status", &"<locked>"),
        };

        s.finish()
    }
}

impl ChildHandle for SpawnedChildHandle {
    fn id(&self) -> &str {
        &self.id
    }

    fn status(&self) -> Status {
        self.child
            .lock()
            .map(|c| c.status())
            .unwrap_or(Status::Error)
    }

    fn run_sync(&mut self, input: serde_json::Value) -> Result<ChildResult, RunError> {
        let mut child = self
            .child
            .lock()
            .map_err(|e| RunError::ExecutionFailed(format!("lock failed: {}", e)))?;

        Ok(child.run(input))
    }

    fn abort(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            child.abort();
        }
    }

    fn is_finished(&self) -> bool {
        self.child
            .lock()
            .map(|c| {
                let status = c.status();
                matches!(status, Status::Completed | Status::Error | Status::Aborted)
            })
            .unwrap_or(true) // If lock fails, treat as finished
    }
}

#[async_trait]
impl AsyncChildHandle for SpawnedChildHandle {
    fn id(&self) -> &str {
        &self.id
    }

    fn status(&self) -> Status {
        self.child
            .lock()
            .map(|c| c.status())
            .unwrap_or(Status::Error)
    }

    async fn run(&mut self, input: serde_json::Value) -> Result<ChildResult, RunError> {
        // Clone Arc for move into spawn_blocking
        let child = Arc::clone(&self.child);

        // Run synchronous child in blocking task
        tokio::task::spawn_blocking(move || {
            let mut guard = child
                .lock()
                .map_err(|e| RunError::ExecutionFailed(format!("lock failed: {}", e)))?;
            Ok(guard.run(input))
        })
        .await
        .map_err(|e| RunError::ExecutionFailed(format!("task join failed: {}", e)))?
    }

    fn abort(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            child.abort();
        }
    }

    fn is_finished(&self) -> bool {
        self.child
            .lock()
            .map(|c| {
                let status = c.status();
                matches!(status, Status::Completed | Status::Error | Status::Aborted)
            })
            .unwrap_or(true)
    }
}

/// Spawner for managing children within a Runner.
pub struct ChildSpawner {
    /// Spawned children by ID.
    children: HashMap<String, ManagedChild>,
    /// Maximum allowed children.
    max_children: usize,
    /// Parent ID (Component or Child that owns this spawner).
    parent_id: String,
    /// Output sender (for child output to parent).
    output_tx: mpsc::Sender<super::Event>,
}

impl ChildSpawner {
    /// Creates a new ChildSpawner.
    ///
    /// # Arguments
    ///
    /// * `parent_id` - ID of the parent Component/Child
    /// * `output_tx` - Sender for output events
    #[must_use]
    pub fn new(parent_id: impl Into<String>, output_tx: mpsc::Sender<super::Event>) -> Self {
        Self {
            children: HashMap::new(),
            max_children: DEFAULT_MAX_CHILDREN,
            parent_id: parent_id.into(),
            output_tx,
        }
    }

    /// Sets the maximum number of children.
    #[must_use]
    pub fn with_max_children(mut self, max: usize) -> Self {
        self.max_children = max;
        self
    }

    /// Returns the number of active children.
    #[must_use]
    pub fn child_count(&self) -> usize {
        self.children.len()
    }

    /// Returns the maximum allowed children.
    #[must_use]
    pub fn max_children(&self) -> usize {
        self.max_children
    }

    /// Returns the parent ID.
    #[must_use]
    pub fn parent_id(&self) -> &str {
        &self.parent_id
    }

    /// Internal spawn logic shared by `spawn()` and `spawn_async()`.
    fn spawn_inner(
        &mut self,
        config: ChildConfig,
        child: Box<dyn RunnableChild>,
    ) -> Result<SpawnedChildHandle, SpawnError> {
        // Check limits
        if self.children.len() >= self.max_children {
            return Err(SpawnError::MaxChildrenReached(self.max_children));
        }

        // Check for duplicate ID
        if self.children.contains_key(&config.id) {
            return Err(SpawnError::AlreadyExists(config.id.clone()));
        }

        let id = config.id.clone();

        // Wrap child in Arc<Mutex> for sharing
        let child_arc = Arc::new(Mutex::new(child));

        // Create managed child
        let managed = ManagedChild {
            child: Arc::clone(&child_arc),
        };

        // Store managed child
        self.children.insert(id.clone(), managed);

        Ok(SpawnedChildHandle {
            id,
            child: child_arc,
        })
    }

    /// Spawns a child and returns a handle.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration for the child
    /// * `child` - The RunnableChild instance
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - Max children reached
    /// - Child with same ID already exists
    pub fn spawn(
        &mut self,
        config: ChildConfig,
        child: Box<dyn RunnableChild>,
    ) -> Result<Box<dyn ChildHandle>, SpawnError> {
        Ok(Box::new(self.spawn_inner(config, child)?))
    }

    /// Spawns a child and returns an async handle.
    ///
    /// Unlike [`spawn()`], this returns a handle that properly uses
    /// `spawn_blocking` for async execution.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration for the child
    /// * `child` - The RunnableChild instance
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - Max children reached
    /// - Child with same ID already exists
    pub fn spawn_async(
        &mut self,
        config: ChildConfig,
        child: Box<dyn RunnableChild>,
    ) -> Result<Box<dyn AsyncChildHandle>, SpawnError> {
        Ok(Box::new(self.spawn_inner(config, child)?))
    }

    /// Propagates a signal to all children.
    pub fn propagate_signal(&mut self, signal: &Signal) {
        for (id, managed) in &mut self.children {
            if let Ok(mut child) = managed.child.lock() {
                let response = child.on_signal(signal);
                if matches!(response, orcs_event::SignalResponse::Abort) {
                    tracing::debug!("Child {} aborted on signal", id);
                }
            }
        }
    }

    /// Aborts all children.
    pub fn abort_all(&mut self) {
        for (id, managed) in &mut self.children {
            if let Ok(mut child) = managed.child.lock() {
                child.abort();
                tracing::debug!("Aborted child {}", id);
            }
        }
    }

    /// Removes finished children and returns their results.
    pub fn reap_finished(&mut self) -> Vec<(String, Status)> {
        let mut finished = Vec::new();

        self.children.retain(|id, managed| {
            let status = managed
                .child
                .lock()
                .map(|c| c.status())
                .unwrap_or(Status::Error);

            if matches!(status, Status::Completed | Status::Error | Status::Aborted) {
                finished.push((id.clone(), status));
                false // Remove from map
            } else {
                true // Keep in map
            }
        });

        finished
    }

    /// Returns the IDs of all active children.
    #[must_use]
    pub fn child_ids(&self) -> Vec<String> {
        self.children.keys().cloned().collect()
    }

    /// Returns the output sender.
    #[must_use]
    pub fn output_tx(&self) -> &mpsc::Sender<super::Event> {
        &self.output_tx
    }

    /// Runs a child by ID with the given input.
    ///
    /// # Arguments
    ///
    /// * `id` - The child ID
    /// * `input` - Input data to pass to the child
    ///
    /// # Returns
    ///
    /// The child's result, or an error if:
    /// - Child not found
    /// - Child lock failed
    pub fn run_child(
        &self,
        id: &str,
        input: serde_json::Value,
    ) -> Result<ChildResult, orcs_component::RunError> {
        let managed = self.children.get(id).ok_or_else(|| {
            orcs_component::RunError::ExecutionFailed(format!("child not found: {}", id))
        })?;

        let mut child = managed.child.lock().map_err(|e| {
            orcs_component::RunError::ExecutionFailed(format!("child lock failed: {}", e))
        })?;

        Ok(child.run(input))
    }
}

impl Debug for ChildSpawner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildSpawner")
            .field("parent_id", &self.parent_id)
            .field("child_count", &self.children.len())
            .field("max_children", &self.max_children)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_component::{Child, Identifiable, SignalReceiver, Statusable};
    use orcs_event::SignalResponse;
    use serde_json::json;

    /// Test implementation of RunnableChild.
    struct TestWorker {
        id: String,
        status: Status,
    }

    impl TestWorker {
        fn new(id: &str) -> Self {
            Self {
                id: id.into(),
                status: Status::Idle,
            }
        }
    }

    impl Identifiable for TestWorker {
        fn id(&self) -> &str {
            &self.id
        }
    }

    impl SignalReceiver for TestWorker {
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

    impl Statusable for TestWorker {
        fn status(&self) -> Status {
            self.status
        }
    }

    impl Child for TestWorker {}

    impl RunnableChild for TestWorker {
        fn run(&mut self, input: serde_json::Value) -> ChildResult {
            self.status = Status::Running;
            // Simple echo
            self.status = Status::Idle;
            ChildResult::Ok(input)
        }
    }

    fn setup() -> ChildSpawner {
        let (output_tx, _) = mpsc::channel(64);
        ChildSpawner::new("test-parent", output_tx)
    }

    #[test]
    fn spawn_child() {
        let mut spawner = setup();
        let config = ChildConfig::new("worker-1");
        let worker = Box::new(TestWorker::new("worker-1"));

        let result = spawner.spawn(config, worker);
        assert!(result.is_ok());
        assert_eq!(spawner.child_count(), 1);
    }

    #[test]
    fn spawn_duplicate_fails() {
        let mut spawner = setup();

        let config1 = ChildConfig::new("worker-1");
        let worker1 = Box::new(TestWorker::new("worker-1"));
        spawner.spawn(config1, worker1).unwrap();

        let config2 = ChildConfig::new("worker-1");
        let worker2 = Box::new(TestWorker::new("worker-1"));
        let result = spawner.spawn(config2, worker2);

        assert!(matches!(result, Err(SpawnError::AlreadyExists(_))));
    }

    #[test]
    fn spawn_max_children_limit() {
        let mut spawner = setup().with_max_children(2);

        for i in 0..2 {
            let config = ChildConfig::new(format!("worker-{}", i));
            let worker = Box::new(TestWorker::new(&format!("worker-{}", i)));
            spawner.spawn(config, worker).unwrap();
        }

        let config = ChildConfig::new("worker-overflow");
        let worker = Box::new(TestWorker::new("worker-overflow"));
        let result = spawner.spawn(config, worker);

        assert!(matches!(result, Err(SpawnError::MaxChildrenReached(2))));
    }

    #[test]
    fn child_handle_run() {
        let mut spawner = setup();
        let config = ChildConfig::new("worker-1");
        let worker = Box::new(TestWorker::new("worker-1"));

        let mut handle = spawner.spawn(config, worker).unwrap();
        let result = handle.run_sync(json!({"test": true})).unwrap();

        assert!(result.is_ok());
        if let ChildResult::Ok(data) = result {
            assert_eq!(data["test"], true);
        }
    }

    #[test]
    fn child_handle_status() {
        let mut spawner = setup();
        let config = ChildConfig::new("worker-1");
        let worker = Box::new(TestWorker::new("worker-1"));

        let handle = spawner.spawn(config, worker).unwrap();
        assert_eq!(handle.status(), Status::Idle);
    }

    #[test]
    fn child_handle_abort() {
        let mut spawner = setup();
        let config = ChildConfig::new("worker-1");
        let worker = Box::new(TestWorker::new("worker-1"));

        let mut handle = spawner.spawn(config, worker).unwrap();
        handle.abort();

        // Status should be aborted
        assert!(handle.is_finished());
    }

    #[test]
    fn abort_all() {
        let mut spawner = setup();

        for i in 0..3 {
            let config = ChildConfig::new(format!("worker-{}", i));
            let worker = Box::new(TestWorker::new(&format!("worker-{}", i)));
            spawner.spawn(config, worker).unwrap();
        }

        spawner.abort_all();

        // All children should be aborted
        let finished = spawner.reap_finished();
        assert_eq!(finished.len(), 3);
        for (_, status) in finished {
            assert_eq!(status, Status::Aborted);
        }
    }

    #[test]
    fn child_ids() {
        let mut spawner = setup();

        for i in 0..3 {
            let config = ChildConfig::new(format!("worker-{}", i));
            let worker = Box::new(TestWorker::new(&format!("worker-{}", i)));
            spawner.spawn(config, worker).unwrap();
        }

        let ids = spawner.child_ids();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&"worker-0".to_string()));
        assert!(ids.contains(&"worker-1".to_string()));
        assert!(ids.contains(&"worker-2".to_string()));
    }

    // --- AsyncChildHandle tests ---

    /// Helper to create SpawnedChildHandle directly for async testing.
    fn create_spawned_handle(id: &str) -> SpawnedChildHandle {
        let worker = Box::new(TestWorker::new(id)) as Box<dyn RunnableChild>;
        SpawnedChildHandle {
            id: id.to_string(),
            child: Arc::new(Mutex::new(worker)),
        }
    }

    #[tokio::test]
    async fn async_child_handle_run() {
        let mut handle = create_spawned_handle("async-worker-1");

        let result = AsyncChildHandle::run(&mut handle, json!({"async": true}))
            .await
            .unwrap();

        assert!(result.is_ok());
        if let ChildResult::Ok(data) = result {
            assert_eq!(data["async"], true);
        }
    }

    #[tokio::test]
    async fn async_child_handle_status() {
        let handle = create_spawned_handle("async-worker");

        assert_eq!(AsyncChildHandle::status(&handle), Status::Idle);
        assert_eq!(AsyncChildHandle::id(&handle), "async-worker");
    }

    #[tokio::test]
    async fn async_child_handle_abort() {
        let mut handle = create_spawned_handle("async-worker");

        AsyncChildHandle::abort(&mut handle);

        assert!(AsyncChildHandle::is_finished(&handle));
    }

    #[tokio::test]
    async fn async_child_handle_object_safety() {
        let handle = create_spawned_handle("async-boxed");

        // Can be used as Box<dyn AsyncChildHandle>
        let boxed: Box<dyn AsyncChildHandle> = Box::new(handle);

        assert_eq!(boxed.id(), "async-boxed");
        assert_eq!(boxed.status(), Status::Idle);
    }
}

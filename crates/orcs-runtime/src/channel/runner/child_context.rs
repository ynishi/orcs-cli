//! ChildContext implementation for Runner.
//!
//! Provides the runtime context that is injected into Children
//! to enable them to interact with the system safely.

use super::child_spawner::ChildSpawner;
use super::Event;
use orcs_component::{
    async_trait, AsyncChildContext, AsyncChildHandle, ChildConfig, ChildContext, ChildHandle,
    ChildResult, RunError, SpawnError,
};
use orcs_event::EventCategory;
use orcs_types::ComponentId;
use std::fmt::Debug;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Implementation of ChildContext for the Runner.
///
/// This is injected into Children to provide safe access to:
/// - Output emission (to parent/IO)
/// - Child spawning
/// - Status queries
#[derive(Clone)]
pub struct ChildContextImpl {
    /// Parent ID (Component or Child).
    parent_id: String,
    /// Output sender for events.
    output_tx: mpsc::Sender<Event>,
    /// Child spawner (shared, locked).
    spawner: Arc<Mutex<ChildSpawner>>,
    /// Lua loader for creating children from scripts.
    lua_loader: Option<Arc<dyn LuaChildLoader>>,
}

/// Trait for loading Lua children from config.
///
/// This abstraction allows the context to create LuaChild instances
/// without depending directly on orcs-lua.
pub trait LuaChildLoader: Send + Sync {
    /// Creates a RunnableChild from a config.
    fn load(
        &self,
        config: &ChildConfig,
    ) -> Result<Box<dyn orcs_component::RunnableChild>, SpawnError>;
}

impl ChildContextImpl {
    /// Creates a new ChildContextImpl.
    ///
    /// # Arguments
    ///
    /// * `parent_id` - ID of the parent Component/Child
    /// * `output_tx` - Sender for output events
    /// * `spawner` - Shared child spawner
    #[must_use]
    pub fn new(
        parent_id: impl Into<String>,
        output_tx: mpsc::Sender<Event>,
        spawner: Arc<Mutex<ChildSpawner>>,
    ) -> Self {
        Self {
            parent_id: parent_id.into(),
            output_tx,
            spawner,
            lua_loader: None,
        }
    }

    /// Sets the Lua child loader.
    #[must_use]
    pub fn with_lua_loader(mut self, loader: Arc<dyn LuaChildLoader>) -> Self {
        self.lua_loader = Some(loader);
        self
    }

    /// Creates an output event.
    fn create_output_event(&self, message: &str, level: &str) -> Event {
        Event {
            category: EventCategory::Output,
            operation: "display".to_string(),
            source: ComponentId::child(&self.parent_id),
            payload: serde_json::json!({
                "message": message,
                "level": level,
                "source": self.parent_id,
            }),
        }
    }
}

impl Debug for ChildContextImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildContextImpl")
            .field("parent_id", &self.parent_id)
            .field("has_lua_loader", &self.lua_loader.is_some())
            .finish()
    }
}

impl ChildContext for ChildContextImpl {
    fn parent_id(&self) -> &str {
        &self.parent_id
    }

    fn emit_output(&self, message: &str) {
        let event = self.create_output_event(message, "info");
        let _ = self.output_tx.try_send(event);
    }

    fn emit_output_with_level(&self, message: &str, level: &str) {
        let event = self.create_output_event(message, level);
        let _ = self.output_tx.try_send(event);
    }

    fn spawn_child(&self, config: ChildConfig) -> Result<Box<dyn ChildHandle>, SpawnError> {
        // Get Lua loader
        let loader = self
            .lua_loader
            .as_ref()
            .ok_or_else(|| SpawnError::Internal("no lua loader configured".into()))?;

        // Load child from config
        let child = loader.load(&config)?;

        // Spawn via spawner
        let mut spawner = self
            .spawner
            .lock()
            .map_err(|e| SpawnError::Internal(format!("spawner lock failed: {}", e)))?;

        spawner.spawn(config, child)
    }

    fn child_count(&self) -> usize {
        self.spawner.lock().map(|s| s.child_count()).unwrap_or(0)
    }

    fn max_children(&self) -> usize {
        self.spawner.lock().map(|s| s.max_children()).unwrap_or(0)
    }

    fn send_to_child(
        &self,
        child_id: &str,
        input: serde_json::Value,
    ) -> Result<ChildResult, RunError> {
        let spawner = self
            .spawner
            .lock()
            .map_err(|e| RunError::ExecutionFailed(format!("spawner lock failed: {}", e)))?;

        spawner.run_child(child_id, input)
    }

    fn clone_box(&self) -> Box<dyn ChildContext> {
        Box::new(self.clone())
    }
}

#[async_trait]
impl AsyncChildContext for ChildContextImpl {
    fn parent_id(&self) -> &str {
        &self.parent_id
    }

    fn emit_output(&self, message: &str) {
        let event = self.create_output_event(message, "info");
        if let Err(e) = self.output_tx.try_send(event) {
            tracing::warn!("Failed to emit output: {}", e);
        }
    }

    fn emit_output_with_level(&self, message: &str, level: &str) {
        let event = self.create_output_event(message, level);
        if let Err(e) = self.output_tx.try_send(event) {
            tracing::warn!("Failed to emit output: {}", e);
        }
    }

    async fn spawn_child(
        &self,
        config: ChildConfig,
    ) -> Result<Box<dyn AsyncChildHandle>, SpawnError> {
        // Get Lua loader
        let loader = self
            .lua_loader
            .as_ref()
            .ok_or_else(|| SpawnError::Internal("no lua loader configured".into()))?;

        // Load child from config
        let child = loader.load(&config)?;

        // Spawn via spawner (spawn_async returns Box<dyn AsyncChildHandle>)
        let mut spawner = self
            .spawner
            .lock()
            .map_err(|e| SpawnError::Internal(format!("spawner lock failed: {}", e)))?;

        spawner.spawn_async(config, child)
    }

    fn child_count(&self) -> usize {
        self.spawner.lock().map(|s| s.child_count()).unwrap_or(0)
    }

    fn max_children(&self) -> usize {
        self.spawner.lock().map(|s| s.max_children()).unwrap_or(0)
    }

    fn send_to_child(
        &self,
        child_id: &str,
        input: serde_json::Value,
    ) -> Result<ChildResult, RunError> {
        let spawner = self
            .spawner
            .lock()
            .map_err(|e| RunError::ExecutionFailed(format!("spawner lock failed: {}", e)))?;

        spawner.run_child(child_id, input)
    }

    fn clone_box(&self) -> Box<dyn AsyncChildContext> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_component::{
        Child, ChildResult, Identifiable, RunnableChild, SignalReceiver, Status, Statusable,
    };
    use orcs_event::{Signal, SignalResponse};

    /// Test worker implementation.
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
            ChildResult::Ok(input)
        }
    }

    /// Test loader that creates TestWorker.
    struct TestLoader;

    impl LuaChildLoader for TestLoader {
        fn load(&self, config: &ChildConfig) -> Result<Box<dyn RunnableChild>, SpawnError> {
            Ok(Box::new(TestWorker::new(&config.id)))
        }
    }

    fn setup() -> (ChildContextImpl, mpsc::Receiver<Event>) {
        let (output_tx, output_rx) = mpsc::channel(64);

        let spawner = ChildSpawner::new("test-parent", output_tx.clone());
        let spawner_arc = Arc::new(Mutex::new(spawner));

        let ctx = ChildContextImpl::new("test-parent", output_tx, spawner_arc)
            .with_lua_loader(Arc::new(TestLoader));

        (ctx, output_rx)
    }

    #[test]
    fn parent_id() {
        let (ctx, _) = setup();
        assert_eq!(ChildContext::parent_id(&ctx), "test-parent");
    }

    #[test]
    fn emit_output() {
        let (ctx, mut output_rx) = setup();

        ChildContext::emit_output(&ctx, "Hello, World!");

        let event = output_rx.try_recv().unwrap();
        assert_eq!(event.category, EventCategory::Output);
        assert_eq!(event.payload["message"], "Hello, World!");
        assert_eq!(event.payload["level"], "info");
    }

    #[test]
    fn emit_output_with_level() {
        let (ctx, mut output_rx) = setup();

        ChildContext::emit_output_with_level(&ctx, "Warning message", "warn");

        let event = output_rx.try_recv().unwrap();
        assert_eq!(event.payload["message"], "Warning message");
        assert_eq!(event.payload["level"], "warn");
    }

    #[test]
    fn spawn_child() {
        let (ctx, _) = setup();

        let config = ChildConfig::new("worker-1");
        let result = ChildContext::spawn_child(&ctx, config);

        assert!(result.is_ok());
        assert_eq!(ChildContext::child_count(&ctx), 1);
    }

    #[test]
    fn max_children() {
        let (ctx, _) = setup();
        assert!(ChildContext::max_children(&ctx) > 0);
    }

    #[test]
    fn clone_box() {
        let (ctx, _) = setup();
        let cloned: Box<dyn ChildContext> = ChildContext::clone_box(&ctx);

        assert_eq!(cloned.parent_id(), "test-parent");
    }

    // --- AsyncChildContext tests ---

    #[tokio::test]
    async fn async_spawn_child() {
        let (ctx, _) = setup();

        let config = ChildConfig::new("async-worker-1");
        let result = AsyncChildContext::spawn_child(&ctx, config).await;

        assert!(result.is_ok());
        assert_eq!(AsyncChildContext::child_count(&ctx), 1);
    }

    #[tokio::test]
    async fn async_spawn_child_and_run() {
        let (ctx, _) = setup();

        let config = ChildConfig::new("async-worker-2");
        let mut handle = AsyncChildContext::spawn_child(&ctx, config).await.unwrap();

        let result = handle.run(serde_json::json!({"test": true})).await;

        assert!(result.is_ok());
        if let Ok(ChildResult::Ok(data)) = result {
            assert_eq!(data["test"], true);
        }
    }

    #[tokio::test]
    async fn async_emit_output() {
        let (ctx, mut output_rx) = setup();

        AsyncChildContext::emit_output(&ctx, "Async Hello!");

        let event = output_rx.try_recv().unwrap();
        assert_eq!(event.payload["message"], "Async Hello!");
    }

    #[tokio::test]
    async fn async_clone_box() {
        let (ctx, _) = setup();
        let cloned: Box<dyn AsyncChildContext> = AsyncChildContext::clone_box(&ctx);

        assert_eq!(AsyncChildContext::parent_id(cloned.as_ref()), "test-parent");
    }
}

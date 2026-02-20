//! Integration tests for Child spawning system.
//!
//! Tests the complete flow of:
//! - ChannelRunner with ChildSpawner
//! - ChildContext creation and usage
//! - Signal propagation to children
//! - Child lifecycle management

use orcs_component::{
    Child, ChildConfig, ChildContext, ChildResult, Component, ComponentError, Identifiable,
    RunnableChild, SignalReceiver, SpawnError, Status, Statusable,
};
use orcs_event::{EventCategory, Signal, SignalResponse};
use orcs_runtime::{
    ChannelConfig, ChannelRunner, ChildContextImpl, ChildSpawner, LuaChildLoader, OutputReceiver,
    OutputSender, World, WorldCommand, WorldManager,
};
use orcs_types::{ComponentId, Principal};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc, RwLock};

// =============================================================================
// Test Fixtures
// =============================================================================

/// Test worker child implementation.
struct TestWorker {
    id: String,
    status: Status,
    run_count: Arc<AtomicUsize>,
}

impl TestWorker {
    fn new(id: &str) -> Self {
        Self {
            id: id.into(),
            status: Status::Idle,
            run_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn with_counter(id: &str, counter: Arc<AtomicUsize>) -> Self {
        Self {
            id: id.into(),
            status: Status::Idle,
            run_count: counter,
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
        self.run_count.fetch_add(1, Ordering::SeqCst);
        self.status = Status::Idle;
        ChildResult::Ok(input)
    }
}

/// Test component that can use ChildSpawner.
struct SpawnerComponent {
    id: ComponentId,
    status: Status,
}

impl SpawnerComponent {
    fn new(name: &str) -> Self {
        Self {
            id: ComponentId::builtin(name),
            status: Status::Idle,
        }
    }
}

impl Component for SpawnerComponent {
    fn id(&self) -> &ComponentId {
        &self.id
    }

    fn status(&self) -> Status {
        self.status
    }

    fn on_request(&mut self, request: &orcs_event::Request) -> Result<Value, ComponentError> {
        Ok(request.payload.clone())
    }

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

/// Test loader that creates TestWorker instances.
struct TestLoader {
    create_count: Arc<AtomicUsize>,
}

impl TestLoader {
    fn new() -> Self {
        Self {
            create_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn with_counter(counter: Arc<AtomicUsize>) -> Self {
        Self {
            create_count: counter,
        }
    }
}

impl LuaChildLoader for TestLoader {
    fn load(&self, config: &ChildConfig) -> Result<Box<dyn RunnableChild>, SpawnError> {
        self.create_count.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(TestWorker::new(&config.id)))
    }
}

// =============================================================================
// ChildSpawner Integration Tests
// =============================================================================

mod child_spawner_integration {
    use super::*;

    fn setup_spawner() -> (ChildSpawner, OutputReceiver) {
        let (output_tx, output_rx) = OutputSender::channel(64);
        let spawner = ChildSpawner::new("test-parent", output_tx);
        (spawner, output_rx)
    }

    #[test]
    fn spawn_and_run_child() {
        let (mut spawner, _) = setup_spawner();

        let config = ChildConfig::new("worker-1");
        let run_count = Arc::new(AtomicUsize::new(0));
        let worker = Box::new(TestWorker::with_counter("worker-1", Arc::clone(&run_count)));

        let mut handle = spawner.spawn(config, worker).expect("spawn");

        // Run the child
        let result = handle.run_sync(json!({"value": 42}));
        assert!(result.is_ok());

        let child_result = result.unwrap();
        assert!(child_result.is_ok());

        // Verify run was called
        assert_eq!(run_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn signal_propagation_to_children() {
        let (mut spawner, _) = setup_spawner();

        // Spawn multiple children
        for i in 0..3 {
            let id = format!("worker-{}", i);
            let config = ChildConfig::new(&id);
            let worker = Box::new(TestWorker::new(&id));
            spawner.spawn(config, worker).expect("spawn");
        }

        assert_eq!(spawner.child_count(), 3);

        // Send a veto signal
        let veto = Signal::veto(Principal::System);
        spawner.propagate_signal(&veto);

        // All children should be aborted (status updates via propagate_signal)
        // Note: The status update happens through the child's on_signal -> Abort
        // which sets internal status but watch channel update requires spawner logic
    }

    #[test]
    fn abort_all_children() {
        let (mut spawner, _) = setup_spawner();

        // Spawn children
        for i in 0..3 {
            let id = format!("worker-{}", i);
            let config = ChildConfig::new(&id);
            let worker = Box::new(TestWorker::new(&id));
            spawner.spawn(config, worker).expect("spawn");
        }

        // Abort all
        spawner.abort_all();

        // Reap finished children
        let finished = spawner.reap_finished();
        assert_eq!(finished.len(), 3);

        for (_, status) in finished {
            assert_eq!(status, Status::Aborted);
        }
    }

    #[test]
    fn max_children_limit() {
        let (spawner, _) = setup_spawner();
        let mut spawner = spawner.with_max_children(2);

        // Spawn up to limit
        for i in 0..2 {
            let id = format!("worker-{}", i);
            let config = ChildConfig::new(&id);
            let worker = Box::new(TestWorker::new(&id));
            spawner.spawn(config, worker).expect("spawn");
        }

        // Try to exceed limit
        let config = ChildConfig::new("overflow");
        let worker = Box::new(TestWorker::new("overflow"));
        let result = spawner.spawn(config, worker);

        assert!(matches!(result, Err(SpawnError::MaxChildrenReached(2))));
    }
}

// =============================================================================
// ChildContext Integration Tests
// =============================================================================

mod child_context_integration {
    use super::*;

    fn setup_context() -> (ChildContextImpl, OutputReceiver) {
        let (output_tx, output_rx) = OutputSender::channel(64);

        let spawner = ChildSpawner::new("parent", output_tx.clone());
        let spawner_arc = Arc::new(Mutex::new(spawner));

        let ctx = ChildContextImpl::new("child-1", output_tx, spawner_arc)
            .with_lua_loader(Arc::new(TestLoader::new()));

        (ctx, output_rx)
    }

    #[test]
    fn emit_output() {
        let (ctx, mut output_rx) = setup_context();

        ctx.emit_output("Hello, World!");

        let event = output_rx.try_recv().expect("receive event");
        assert_eq!(event.category, EventCategory::Output);
        assert_eq!(event.payload["message"], "Hello, World!");
        assert_eq!(event.payload["level"], "info");
    }

    #[test]
    fn emit_output_with_level() {
        let (ctx, mut output_rx) = setup_context();

        ctx.emit_output_with_level("Warning!", "warn");

        let event = output_rx.try_recv().expect("receive event");
        assert_eq!(event.payload["message"], "Warning!");
        assert_eq!(event.payload["level"], "warn");
    }

    #[test]
    fn spawn_child_via_context() {
        let (ctx, _) = setup_context();

        let config = ChildConfig::new("sub-child-1");
        let result = ctx.spawn_child(config);

        assert!(result.is_ok());
        let handle = result.unwrap();
        assert_eq!(handle.id(), "sub-child-1");

        assert_eq!(ctx.child_count(), 1);
    }

    #[test]
    fn child_count_and_max() {
        let (ctx, _) = setup_context();

        assert_eq!(ctx.child_count(), 0);
        assert!(ctx.max_children() > 0);

        // Spawn children
        ctx.spawn_child(ChildConfig::new("child-1")).unwrap();
        ctx.spawn_child(ChildConfig::new("child-2")).unwrap();

        assert_eq!(ctx.child_count(), 2);
    }

    #[test]
    fn clone_box() {
        let (ctx, _) = setup_context();

        let cloned = ctx.clone_box();

        assert_eq!(cloned.parent_id(), "child-1");
        assert_eq!(cloned.child_count(), ctx.child_count());
    }
}

// =============================================================================
// ChannelRunner with ChildSpawner Tests
// =============================================================================

mod runner_integration {
    use super::*;

    async fn setup() -> (
        tokio::task::JoinHandle<()>,
        mpsc::Sender<WorldCommand>,
        Arc<RwLock<World>>,
        broadcast::Sender<Signal>,
        orcs_types::ChannelId,
    ) {
        let mut world = World::new();
        let io = world.create_channel(ChannelConfig::interactive());

        let (manager, world_tx) = WorldManager::with_world(world);
        let world_handle = manager.world();

        let manager_task = tokio::spawn(manager.run());

        let (signal_tx, _) = broadcast::channel(64);

        (manager_task, world_tx, world_handle, signal_tx, io)
    }

    async fn teardown(
        manager_task: tokio::task::JoinHandle<()>,
        world_tx: mpsc::Sender<WorldCommand>,
    ) {
        let _ = world_tx.send(WorldCommand::Shutdown).await;
        let _ = manager_task.await;
    }

    #[tokio::test]
    async fn runner_with_child_spawner_creation() {
        let (manager_task, world_tx, world, signal_tx, channel_id) = setup().await;

        let signal_rx = signal_tx.subscribe();
        let component = Box::new(SpawnerComponent::new("spawner-test"));

        let (runner, handle) =
            ChannelRunner::builder(channel_id, world_tx.clone(), world, signal_rx, component)
                .with_child_spawner(None)
                .build();

        assert_eq!(runner.id(), channel_id);
        assert!(runner.child_spawner().is_some());

        drop(handle);
        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn create_child_context_from_runner() {
        let (manager_task, world_tx, world, signal_tx, channel_id) = setup().await;

        let signal_rx = signal_tx.subscribe();
        let component = Box::new(SpawnerComponent::new("context-test"));

        let (runner, _handle) =
            ChannelRunner::builder(channel_id, world_tx.clone(), world, signal_rx, component)
                .with_child_spawner(None)
                .build();

        // Create context without loader
        let ctx = runner.create_child_context("test-child");
        assert!(ctx.is_some());

        let ctx = ctx.unwrap();
        assert_eq!(ctx.parent_id(), "test-child");

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn create_child_context_with_loader() {
        let (manager_task, world_tx, world, signal_tx, channel_id) = setup().await;

        let signal_rx = signal_tx.subscribe();
        let component = Box::new(SpawnerComponent::new("loader-test"));

        let (runner, _handle) =
            ChannelRunner::builder(channel_id, world_tx.clone(), world, signal_rx, component)
                .with_child_spawner(None)
                .build();

        let create_count = Arc::new(AtomicUsize::new(0));
        let loader = Arc::new(TestLoader::with_counter(Arc::clone(&create_count)));

        let ctx = runner.create_child_context_with_loader("test-child", loader);
        assert!(ctx.is_some());

        let ctx = ctx.unwrap();

        // Spawn a child via context
        let result = ctx.spawn_child(ChildConfig::new("spawned-child"));
        assert!(result.is_ok());

        // Verify loader was called
        assert_eq!(create_count.load(Ordering::SeqCst), 1);

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn runner_without_spawner_returns_none() {
        let (manager_task, world_tx, world, signal_tx, channel_id) = setup().await;

        let signal_rx = signal_tx.subscribe();
        let component = Box::new(SpawnerComponent::new("no-spawner"));

        // Use builder without child spawner
        let (runner, _handle) =
            ChannelRunner::builder(channel_id, world_tx.clone(), world, signal_rx, component)
                .build();

        assert!(runner.child_spawner().is_none());
        assert!(runner.create_child_context("test").is_none());

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn runner_with_full_support() {
        let (manager_task, world_tx, world, signal_tx, channel_id) = setup().await;

        let signal_rx = signal_tx.subscribe();
        let component = Box::new(SpawnerComponent::new("full-support"));

        let (runner, handle) =
            ChannelRunner::builder(channel_id, world_tx.clone(), world, signal_rx, component)
                .with_emitter(signal_tx.clone())
                .with_child_spawner(None)
                .build();

        // Should have child spawner
        assert!(runner.child_spawner().is_some());

        // Should be able to create context
        assert!(runner.create_child_context("child").is_some());

        drop(handle);
        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn signal_aborts_spawned_children() {
        let (manager_task, world_tx, world, signal_tx, channel_id) = setup().await;

        let signal_rx = signal_tx.subscribe();
        let component = Box::new(SpawnerComponent::new("signal-test"));

        let (runner, handle) =
            ChannelRunner::builder(channel_id, world_tx.clone(), world, signal_rx, component)
                .with_child_spawner(None)
                .build();

        // Spawn children via spawner directly for testing
        if let Some(spawner) = runner.child_spawner() {
            let mut spawner = spawner.lock().unwrap();
            for i in 0..3 {
                let id = format!("child-{}", i);
                let config = ChildConfig::new(&id);
                let worker = Box::new(TestWorker::new(&id));
                spawner.spawn(config, worker).expect("spawn");
            }
            assert_eq!(spawner.child_count(), 3);
        }

        // Start runner
        let runner_task = tokio::spawn(runner.run());

        // Give runner time to start
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Send veto signal
        signal_tx.send(Signal::veto(Principal::System)).unwrap();

        // Wait for runner to stop
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), runner_task).await;
        assert!(result.is_ok());

        drop(handle);
        teardown(manager_task, world_tx).await;
    }
}

// =============================================================================
// End-to-End Workflow Tests
// =============================================================================

mod e2e_workflow {
    use super::*;

    #[test]
    fn complete_child_spawning_workflow() {
        // 1. Create ChildSpawner
        let (output_tx, mut output_rx) = OutputSender::channel(64);
        let spawner = ChildSpawner::new("parent-component", output_tx.clone());
        let spawner_arc = Arc::new(Mutex::new(spawner));

        // 2. Create ChildContext
        let loader = Arc::new(TestLoader::new());
        let ctx = ChildContextImpl::new("parent-component", output_tx, Arc::clone(&spawner_arc))
            .with_lua_loader(loader);

        // 3. Spawn child via context
        let config = ChildConfig::new("worker-1");
        let handle = ctx.spawn_child(config).expect("spawn");
        assert_eq!(handle.id(), "worker-1");

        // 4. Verify child count
        assert_eq!(ctx.child_count(), 1);

        // 5. Emit output
        ctx.emit_output("Child spawned successfully");

        // 6. Verify output event
        let event = output_rx.try_recv().expect("receive");
        assert_eq!(event.payload["message"], "Child spawned successfully");

        // 7. Propagate signal to children
        let cancel = Signal::cancel(orcs_types::ChannelId::new(), Principal::System);
        {
            let mut spawner = spawner_arc.lock().unwrap();
            spawner.propagate_signal(&cancel);
        }

        // 8. Abort all children
        {
            let mut spawner = spawner_arc.lock().unwrap();
            spawner.abort_all();
            let finished = spawner.reap_finished();
            assert_eq!(finished.len(), 1);
        }
    }

    #[test]
    fn nested_child_spawning() {
        let (output_tx, _) = OutputSender::channel(64);

        // Parent spawner
        let parent_spawner = ChildSpawner::new("root", output_tx.clone());
        let parent_spawner_arc = Arc::new(Mutex::new(parent_spawner));

        // Create parent context
        let parent_loader = Arc::new(TestLoader::new());
        let parent_ctx =
            ChildContextImpl::new("root", output_tx.clone(), Arc::clone(&parent_spawner_arc))
                .with_lua_loader(parent_loader);

        // Spawn first-level child
        let child1 = parent_ctx.spawn_child(ChildConfig::new("child-1")).unwrap();
        assert_eq!(child1.id(), "child-1");

        // Create child spawner for second level
        let child_spawner = ChildSpawner::new("child-1", output_tx.clone());
        let child_spawner_arc = Arc::new(Mutex::new(child_spawner));

        // Create child context for spawning grandchildren
        let child_loader = Arc::new(TestLoader::new());
        let child_ctx = ChildContextImpl::new("child-1", output_tx, Arc::clone(&child_spawner_arc))
            .with_lua_loader(child_loader);

        // Spawn grandchild
        let grandchild = child_ctx
            .spawn_child(ChildConfig::new("grandchild-1"))
            .unwrap();
        assert_eq!(grandchild.id(), "grandchild-1");

        // Verify hierarchy
        assert_eq!(parent_ctx.child_count(), 1);
        assert_eq!(child_ctx.child_count(), 1);
    }
}

// =============================================================================
// spawn_runner Registration Tests (World + handles + map)
// =============================================================================

mod spawn_runner_registration {
    use super::*;
    use orcs_runtime::engine::{SharedChannelHandles, SharedComponentChannelMap};
    use parking_lot::RwLock as PLRwLock;
    use std::collections::HashMap;
    use std::time::Duration;

    /// Creates the full infrastructure needed to test spawn_runner:
    /// WorldManager, shared_handles, component_channel_map, and a
    /// parent ChildContextImpl that has all RPC + runner support wired up.
    async fn setup_spawn_runner_env() -> (
        tokio::task::JoinHandle<()>,
        mpsc::Sender<WorldCommand>,
        Arc<RwLock<World>>,
        broadcast::Sender<Signal>,
        orcs_types::ChannelId,
        SharedChannelHandles,
        SharedComponentChannelMap,
    ) {
        let mut world = World::new();
        let parent_channel = world.create_channel(ChannelConfig::interactive());

        let (manager, world_tx) = WorldManager::with_world(world);
        let world_handle = manager.world();
        let manager_task = tokio::spawn(manager.run());

        let (signal_tx, _) = broadcast::channel(64);

        let shared_handles: SharedChannelHandles = Arc::new(PLRwLock::new(HashMap::new()));
        let component_channel_map: SharedComponentChannelMap =
            Arc::new(PLRwLock::new(HashMap::new()));

        (
            manager_task,
            world_tx,
            world_handle,
            signal_tx,
            parent_channel,
            shared_handles,
            component_channel_map,
        )
    }

    fn create_parent_context(
        parent_channel: orcs_types::ChannelId,
        world_tx: &mpsc::Sender<WorldCommand>,
        world: &Arc<RwLock<World>>,
        signal_tx: &broadcast::Sender<Signal>,
        shared_handles: &SharedChannelHandles,
        component_channel_map: &SharedComponentChannelMap,
    ) -> ChildContextImpl {
        let (output_tx, _output_rx) = OutputSender::channel(64);
        let spawner = ChildSpawner::new("test-parent", output_tx.clone());
        let spawner_arc = Arc::new(Mutex::new(spawner));

        // Elevated session so spawn_runner auth check passes
        let session =
            orcs_runtime::auth::Session::new(Principal::User(orcs_types::PrincipalId::new()))
                .elevate(Duration::from_secs(60));

        ChildContextImpl::new("test-parent", output_tx, spawner_arc)
            .with_runner_support(world_tx.clone(), Arc::clone(world), signal_tx.clone())
            .with_rpc_support(
                Arc::clone(shared_handles),
                Arc::clone(component_channel_map),
                parent_channel,
            )
            .with_session(session)
            .with_checker(Arc::new(orcs_runtime::auth::DefaultPolicy))
    }

    async fn teardown(
        manager_task: tokio::task::JoinHandle<()>,
        world_tx: mpsc::Sender<WorldCommand>,
    ) {
        let _ = world_tx.send(WorldCommand::Shutdown).await;
        let _ = manager_task.await;
    }

    #[tokio::test]
    async fn spawn_runner_registers_in_shared_handles_and_map() {
        let (
            manager_task,
            world_tx,
            world,
            signal_tx,
            parent_channel,
            shared_handles,
            component_channel_map,
        ) = setup_spawn_runner_env().await;

        let ctx = create_parent_context(
            parent_channel,
            &world_tx,
            &world,
            &signal_tx,
            &shared_handles,
            &component_channel_map,
        );

        let component = Box::new(SpawnerComponent::new("agent-alpha"));
        let (channel_id, ready_rx) = ctx
            .spawn_runner(component)
            .expect("spawn_runner should succeed");

        // Wait for registration to complete via ready notification
        let _ = ready_rx.await;

        // Verify shared_handles contains the spawned channel
        {
            let handles = shared_handles.read();
            assert!(
                handles.contains_key(&channel_id),
                "shared_handles should contain spawned channel {}",
                channel_id
            );
        }

        // Verify component_channel_map contains the FQN mapping
        {
            let map = component_channel_map.read();
            let fqn = "builtin::agent-alpha";
            assert!(
                map.contains_key(fqn),
                "component_channel_map should contain FQN '{}'",
                fqn
            );
            assert_eq!(
                map.get(fqn).copied(),
                Some(channel_id),
                "FQN should map to the correct channel_id"
            );
        }

        // Verify World has the channel registered
        {
            let w = world.read().await;
            assert!(
                w.get(&channel_id).is_some(),
                "World should contain channel {}",
                channel_id
            );
        }

        // Cleanup: send veto to stop the spawned runner
        signal_tx
            .send(Signal::veto(Principal::System))
            .expect("send veto signal");
        tokio::time::sleep(Duration::from_millis(100)).await;

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn spawn_runner_cleans_up_on_exit() {
        let (
            manager_task,
            world_tx,
            world,
            signal_tx,
            parent_channel,
            shared_handles,
            component_channel_map,
        ) = setup_spawn_runner_env().await;

        let ctx = create_parent_context(
            parent_channel,
            &world_tx,
            &world,
            &signal_tx,
            &shared_handles,
            &component_channel_map,
        );

        let component = Box::new(SpawnerComponent::new("ephemeral-agent"));
        let (channel_id, ready_rx) = ctx
            .spawn_runner(component)
            .expect("spawn_runner should succeed");

        // Wait for registration to complete via ready notification
        let _ = ready_rx.await;

        // Confirm registration happened
        assert!(
            shared_handles.read().contains_key(&channel_id),
            "precondition: handle should be registered"
        );
        assert!(
            component_channel_map
                .read()
                .contains_key("builtin::ephemeral-agent"),
            "precondition: FQN should be registered"
        );

        // Send veto to stop the runner → triggers cleanup
        signal_tx
            .send(Signal::veto(Principal::System))
            .expect("send veto signal");
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Verify cleanup: handle removed
        assert!(
            !shared_handles.read().contains_key(&channel_id),
            "shared_handles should NOT contain channel after runner exit"
        );

        // Verify cleanup: FQN removed
        assert!(
            !component_channel_map
                .read()
                .contains_key("builtin::ephemeral-agent"),
            "component_channel_map should NOT contain FQN after runner exit"
        );

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn spawn_runner_without_rpc_support_still_works() {
        let (manager_task, world_tx, world, signal_tx, _parent_channel, _, _) =
            setup_spawn_runner_env().await;

        // Create context WITHOUT rpc_support (no shared_handles / map)
        let (output_tx, _output_rx) = OutputSender::channel(64);
        let spawner = ChildSpawner::new("test-no-rpc", output_tx.clone());
        let spawner_arc = Arc::new(Mutex::new(spawner));

        let session =
            orcs_runtime::auth::Session::new(Principal::User(orcs_types::PrincipalId::new()))
                .elevate(Duration::from_secs(60));

        let ctx = ChildContextImpl::new("test-no-rpc", output_tx, spawner_arc)
            .with_runner_support(world_tx.clone(), Arc::clone(&world), signal_tx.clone())
            .with_session(session)
            .with_checker(Arc::new(orcs_runtime::auth::DefaultPolicy));

        // spawn_runner requires channel_id for parent → this context
        // has no channel_id set, so it should fail gracefully.
        let component = Box::new(SpawnerComponent::new("no-rpc-agent"));
        let result = ctx.spawn_runner(component);

        assert!(
            result.is_err(),
            "spawn_runner without channel_id should return error"
        );
        let err = result.expect_err("expected SpawnError");
        assert!(
            err.to_string().contains("parent channel_id"),
            "error should mention parent channel_id, got: {}",
            err
        );

        teardown(manager_task, world_tx).await;
    }

    #[tokio::test]
    async fn spawn_runner_denied_without_elevated_session() {
        let (manager_task, world_tx, world, signal_tx, parent_channel, handles, map) =
            setup_spawn_runner_env().await;

        let (output_tx, _output_rx) = OutputSender::channel(64);
        let spawner = ChildSpawner::new("test-unpriv", output_tx.clone());
        let spawner_arc = Arc::new(Mutex::new(spawner));

        // Non-elevated session → should be denied
        let session =
            orcs_runtime::auth::Session::new(Principal::User(orcs_types::PrincipalId::new()));

        let ctx = ChildContextImpl::new("test-unpriv", output_tx, spawner_arc)
            .with_runner_support(world_tx.clone(), Arc::clone(&world), signal_tx.clone())
            .with_rpc_support(Arc::clone(&handles), Arc::clone(&map), parent_channel)
            .with_session(session)
            .with_checker(Arc::new(orcs_runtime::auth::DefaultPolicy));

        let component = Box::new(SpawnerComponent::new("denied-agent"));
        let result = ctx.spawn_runner(component);

        assert!(result.is_err(), "should deny non-elevated spawn_runner");
        let err = result.expect_err("expected SpawnError");
        assert!(
            err.to_string().contains("elevated privilege"),
            "error should mention elevated privilege, got: {}",
            err
        );

        teardown(manager_task, world_tx).await;
    }
}

// =============================================================================
// spawn_runner RPC Tests (end-to-end: spawn → register → RPC → response)
// =============================================================================

mod spawn_runner_rpc {
    use super::*;
    use orcs_component::ChildContext;
    use orcs_runtime::engine::{SharedChannelHandles, SharedComponentChannelMap};
    use parking_lot::RwLock as PLRwLock;
    use std::collections::HashMap;
    use std::time::Duration;

    async fn setup_spawn_runner_env() -> (
        tokio::task::JoinHandle<()>,
        mpsc::Sender<WorldCommand>,
        Arc<RwLock<World>>,
        broadcast::Sender<Signal>,
        orcs_types::ChannelId,
        SharedChannelHandles,
        SharedComponentChannelMap,
    ) {
        let mut world = World::new();
        let parent_channel = world.create_channel(ChannelConfig::interactive());

        let (manager, world_tx) = WorldManager::with_world(world);
        let world_handle = manager.world();
        let manager_task = tokio::spawn(manager.run());

        let (signal_tx, _) = broadcast::channel(64);

        let shared_handles: SharedChannelHandles = Arc::new(PLRwLock::new(HashMap::new()));
        let component_channel_map: SharedComponentChannelMap =
            Arc::new(PLRwLock::new(HashMap::new()));

        (
            manager_task,
            world_tx,
            world_handle,
            signal_tx,
            parent_channel,
            shared_handles,
            component_channel_map,
        )
    }

    fn create_parent_context(
        parent_channel: orcs_types::ChannelId,
        world_tx: &mpsc::Sender<WorldCommand>,
        world: &Arc<RwLock<World>>,
        signal_tx: &broadcast::Sender<Signal>,
        shared_handles: &SharedChannelHandles,
        component_channel_map: &SharedComponentChannelMap,
    ) -> ChildContextImpl {
        let (output_tx, _output_rx) = OutputSender::channel(64);
        let spawner = ChildSpawner::new("test-parent", output_tx.clone());
        let spawner_arc = Arc::new(Mutex::new(spawner));

        let session =
            orcs_runtime::auth::Session::new(Principal::User(orcs_types::PrincipalId::new()))
                .elevate(Duration::from_secs(60));

        ChildContextImpl::new("test-parent", output_tx, spawner_arc)
            .with_runner_support(world_tx.clone(), Arc::clone(world), signal_tx.clone())
            .with_rpc_support(
                Arc::clone(shared_handles),
                Arc::clone(component_channel_map),
                parent_channel,
            )
            .with_session(session)
            .with_checker(Arc::new(orcs_runtime::auth::DefaultPolicy))
    }

    async fn teardown(
        manager_task: tokio::task::JoinHandle<()>,
        world_tx: mpsc::Sender<WorldCommand>,
    ) {
        let _ = world_tx.send(WorldCommand::Shutdown).await;
        let _ = manager_task.await;
    }

    /// Spawn a Component via spawn_runner, then verify RPC request
    /// is routed to its on_request and the response echoes back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_runner_rpc_echo() {
        let (
            manager_task,
            world_tx,
            world,
            signal_tx,
            parent_channel,
            shared_handles,
            component_channel_map,
        ) = setup_spawn_runner_env().await;

        let ctx = create_parent_context(
            parent_channel,
            &world_tx,
            &world,
            &signal_tx,
            &shared_handles,
            &component_channel_map,
        );

        let component = Box::new(SpawnerComponent::new("rpc-echo"));
        let (_channel_id, ready_rx) = ctx
            .spawn_runner(component)
            .expect("spawn_runner should succeed");

        // Wait for World + handles + map registration
        let _ = ready_rx.await;

        // Send RPC request via ChildContext::request()
        let result = ctx.request(
            "builtin::rpc-echo",
            "echo",
            json!({"msg": "hello from rpc test"}),
            Some(2000),
        );

        let response = result.expect("RPC should succeed");
        assert_eq!(
            response["msg"], "hello from rpc test",
            "Component should echo the payload back"
        );

        // Cleanup
        signal_tx
            .send(Signal::veto(Principal::System))
            .expect("send veto");
        tokio::time::sleep(Duration::from_millis(100)).await;
        teardown(manager_task, world_tx).await;
    }

    /// Spawn two Components via spawn_runner, verify each responds
    /// independently to RPC requests addressed to its own FQN.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_runner_multiple_components_independent_rpc() {
        let (
            manager_task,
            world_tx,
            world,
            signal_tx,
            parent_channel,
            shared_handles,
            component_channel_map,
        ) = setup_spawn_runner_env().await;

        let ctx = create_parent_context(
            parent_channel,
            &world_tx,
            &world,
            &signal_tx,
            &shared_handles,
            &component_channel_map,
        );

        // Spawn two independent Components
        let comp_a = Box::new(SpawnerComponent::new("worker-alpha"));
        let (id_a, ready_a) = ctx
            .spawn_runner(comp_a)
            .expect("spawn alpha should succeed");

        let comp_b = Box::new(SpawnerComponent::new("worker-beta"));
        let (id_b, ready_b) = ctx.spawn_runner(comp_b).expect("spawn beta should succeed");

        // Wait for both to register
        let _ = ready_a.await;
        let _ = ready_b.await;

        // Verify independent registration
        assert_ne!(id_a, id_b, "channel IDs should be unique");
        assert!(
            component_channel_map
                .read()
                .contains_key("builtin::worker-alpha"),
            "alpha FQN should be registered"
        );
        assert!(
            component_channel_map
                .read()
                .contains_key("builtin::worker-beta"),
            "beta FQN should be registered"
        );

        // RPC to alpha
        let resp_a = ctx
            .request(
                "builtin::worker-alpha",
                "process",
                json!({"from": "alpha"}),
                Some(2000),
            )
            .expect("RPC to alpha should succeed");
        assert_eq!(resp_a["from"], "alpha");

        // RPC to beta
        let resp_b = ctx
            .request(
                "builtin::worker-beta",
                "process",
                json!({"from": "beta"}),
                Some(2000),
            )
            .expect("RPC to beta should succeed");
        assert_eq!(resp_b["from"], "beta");

        // Cleanup
        signal_tx
            .send(Signal::veto(Principal::System))
            .expect("send veto");
        tokio::time::sleep(Duration::from_millis(100)).await;
        teardown(manager_task, world_tx).await;
    }

    /// RPC to a non-existent FQN should return an error containing
    /// "component not found".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_to_unknown_fqn_returns_error() {
        let (
            manager_task,
            world_tx,
            world,
            signal_tx,
            parent_channel,
            shared_handles,
            component_channel_map,
        ) = setup_spawn_runner_env().await;

        let ctx = create_parent_context(
            parent_channel,
            &world_tx,
            &world,
            &signal_tx,
            &shared_handles,
            &component_channel_map,
        );

        // No Component spawned — RPC to non-existent FQN should fail
        let result = ctx.request("builtin::nonexistent", "echo", json!({}), Some(1000));

        assert!(result.is_err(), "RPC to unknown FQN should return error");
        let err = result.expect_err("expected error string");
        assert!(
            err.contains("not found"),
            "error should indicate component not found, got: {}",
            err
        );

        teardown(manager_task, world_tx).await;
    }

    /// After a spawned runner exits (veto), its FQN is cleaned up and
    /// subsequent RPC requests return an error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_after_component_exit_returns_error() {
        let (
            manager_task,
            world_tx,
            world,
            signal_tx,
            parent_channel,
            shared_handles,
            component_channel_map,
        ) = setup_spawn_runner_env().await;

        let ctx = create_parent_context(
            parent_channel,
            &world_tx,
            &world,
            &signal_tx,
            &shared_handles,
            &component_channel_map,
        );

        let component = Box::new(SpawnerComponent::new("ephemeral"));
        let (_channel_id, ready_rx) = ctx
            .spawn_runner(component)
            .expect("spawn_runner should succeed");

        let _ = ready_rx.await;

        // Verify it's reachable while alive
        let result = ctx.request("builtin::ephemeral", "ping", json!({}), Some(2000));
        assert!(
            result.is_ok(),
            "RPC should succeed while Component is alive"
        );

        // Send veto to stop the runner → triggers cleanup
        signal_tx
            .send(Signal::veto(Principal::System))
            .expect("send veto");
        tokio::time::sleep(Duration::from_millis(200)).await;

        // FQN should be cleaned up after runner exit
        assert!(
            !component_channel_map
                .read()
                .contains_key("builtin::ephemeral"),
            "FQN should be removed after runner exit"
        );

        // RPC to stopped Component should fail
        let result = ctx.request("builtin::ephemeral", "ping", json!({}), Some(1000));
        assert!(
            result.is_err(),
            "RPC to stopped Component should return error"
        );

        teardown(manager_task, world_tx).await;
    }
}

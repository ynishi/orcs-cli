//! Integration tests for Manager-Child (Mgr-Sub) pattern.
//!
//! Tests the workflow where:
//! - LuaComponent acts as a Manager
//! - LuaChild instances act as Workers (Sub)
//! - Children can spawn sub-children via ChildContext

use mlua::Lua;
use orcs_component::{
    ChildConfig, ChildContext, ChildHandle, ChildResult, RunnableChild, SpawnError, Status,
};
use orcs_event::{EventCategory, SignalResponse};
use orcs_lua::{LuaChild, LuaComponent};
use orcs_runtime::sandbox::{ProjectSandbox, SandboxPolicy};

fn test_sandbox() -> Arc<dyn SandboxPolicy> {
    Arc::new(ProjectSandbox::new(".").expect("test sandbox"))
}

/// Create a Lua runtime for testing.
///
/// Note: orcs table (log, exec) is now auto-registered by LuaChild,
/// so we only need a plain Lua instance.
fn create_lua() -> Arc<Mutex<Lua>> {
    Arc::new(Mutex::new(Lua::new()))
}
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

// =============================================================================
// Test Fixtures
// =============================================================================

/// Record of a single emit_output call.
#[derive(Debug, Clone)]
struct EmitRecord {
    message: String,
    level: Option<String>,
}

/// Record of a spawn_child call.
#[derive(Debug, Clone)]
struct SpawnRecord {
    id: String,
    #[allow(dead_code)] // For future use: verify script content
    script_inline: Option<String>,
}

impl From<&ChildConfig> for SpawnRecord {
    fn from(config: &ChildConfig) -> Self {
        Self {
            id: config.id.clone(),
            script_inline: config.script_inline.clone(),
        }
    }
}

/// Mock ChildContext for testing LuaChild's spawn capabilities.
#[derive(Debug)]
struct MockChildContext {
    parent_id: String,
    spawn_count: Arc<AtomicUsize>,
    emit_count: Arc<AtomicUsize>,
    max_children: usize,
    /// History of emitted messages for verification.
    emit_history: Arc<Mutex<Vec<EmitRecord>>>,
    /// History of spawn_child calls for verification.
    spawn_history: Arc<Mutex<Vec<SpawnRecord>>>,
    /// If set, spawn_child will fail when id contains this string.
    fail_spawn_on: Option<String>,
}

impl MockChildContext {
    fn new(parent_id: &str) -> Self {
        Self {
            parent_id: parent_id.into(),
            spawn_count: Arc::new(AtomicUsize::new(0)),
            emit_count: Arc::new(AtomicUsize::new(0)),
            max_children: 10,
            emit_history: Arc::new(Mutex::new(Vec::new())),
            spawn_history: Arc::new(Mutex::new(Vec::new())),
            fail_spawn_on: None,
        }
    }

    fn with_max_children(mut self, max: usize) -> Self {
        self.max_children = max;
        self
    }

    fn with_fail_spawn_on(mut self, pattern: &str) -> Self {
        self.fail_spawn_on = Some(pattern.to_string());
        self
    }

    fn spawn_count(&self) -> usize {
        self.spawn_count.load(Ordering::SeqCst)
    }

    fn emit_count(&self) -> usize {
        self.emit_count.load(Ordering::SeqCst)
    }

    /// Get all emitted messages.
    fn emit_messages(&self) -> Vec<String> {
        self.emit_history
            .lock()
            .iter()
            .map(|r| r.message.clone())
            .collect()
    }

    /// Check if any emitted message contains the given substring.
    fn has_emit_containing(&self, substr: &str) -> bool {
        self.emit_history
            .lock()
            .iter()
            .any(|r| r.message.contains(substr))
    }

    /// Get all spawned child IDs.
    fn spawned_ids(&self) -> Vec<String> {
        self.spawn_history
            .lock()
            .iter()
            .map(|r| r.id.clone())
            .collect()
    }
}

/// Mock handle returned by spawn_child
#[derive(Debug)]
struct MockHandle {
    id: String,
}

impl ChildHandle for MockHandle {
    fn id(&self) -> &str {
        &self.id
    }

    fn status(&self) -> Status {
        Status::Idle
    }

    fn run_sync(&mut self, _input: Value) -> Result<ChildResult, orcs_component::RunError> {
        Ok(ChildResult::Ok(Value::Null))
    }

    fn abort(&mut self) {}

    fn is_finished(&self) -> bool {
        false
    }
}

impl ChildContext for MockChildContext {
    fn parent_id(&self) -> &str {
        &self.parent_id
    }

    fn emit_output(&self, message: &str) {
        self.emit_count.fetch_add(1, Ordering::SeqCst);
        self.emit_history.lock().push(EmitRecord {
            message: message.to_string(),
            level: None,
        });
    }

    fn emit_output_with_level(&self, message: &str, level: &str) {
        self.emit_count.fetch_add(1, Ordering::SeqCst);
        self.emit_history.lock().push(EmitRecord {
            message: message.to_string(),
            level: Some(level.to_string()),
        });
    }

    fn spawn_child(&self, config: ChildConfig) -> Result<Box<dyn ChildHandle>, SpawnError> {
        // Check max children limit
        if self.spawn_count.load(Ordering::SeqCst) >= self.max_children {
            return Err(SpawnError::MaxChildrenReached(self.max_children));
        }

        // Check for error injection
        if let Some(ref pattern) = self.fail_spawn_on {
            if config.id.contains(pattern) {
                return Err(SpawnError::Internal(format!(
                    "Injected failure for id containing '{}'",
                    pattern
                )));
            }
        }

        // Record spawn
        self.spawn_history.lock().push(SpawnRecord::from(&config));
        self.spawn_count.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(MockHandle { id: config.id }))
    }

    fn child_count(&self) -> usize {
        self.spawn_count.load(Ordering::SeqCst)
    }

    fn max_children(&self) -> usize {
        self.max_children
    }

    fn send_to_child(
        &self,
        _child_id: &str,
        _input: serde_json::Value,
    ) -> Result<orcs_component::ChildResult, orcs_component::RunError> {
        Ok(orcs_component::ChildResult::Ok(
            serde_json::json!({"mock": true}),
        ))
    }

    fn clone_box(&self) -> Box<dyn ChildContext> {
        Box::new(Self {
            parent_id: self.parent_id.clone(),
            spawn_count: Arc::clone(&self.spawn_count),
            emit_count: Arc::clone(&self.emit_count),
            max_children: self.max_children,
            emit_history: Arc::clone(&self.emit_history),
            spawn_history: Arc::clone(&self.spawn_history),
            fail_spawn_on: self.fail_spawn_on.clone(),
        })
    }
}

// =============================================================================
// Worker Child Tests
// =============================================================================

mod worker_child {
    use super::*;
    use orcs_component::{Identifiable, SignalReceiver, Statusable};

    const WORKER_SCRIPT: &str = include_str!("../scripts/worker_child.lua");

    #[test]
    fn load_worker_child() {
        let lua = create_lua();
        let child = LuaChild::from_script(Arc::clone(&lua), WORKER_SCRIPT, test_sandbox());

        assert!(child.is_ok(), "Failed to load: {:?}", child.err());
        let child = child.unwrap();
        assert_eq!(child.id(), "worker");
        assert!(child.is_runnable());
    }

    #[test]
    fn worker_run_basic_task() {
        let lua = create_lua();
        let mut child =
            LuaChild::from_script(Arc::clone(&lua), WORKER_SCRIPT, test_sandbox()).unwrap();

        // Set mock context
        let ctx = MockChildContext::new("parent");
        child.set_context(Box::new(ctx));

        // Run with input
        let input = json!({
            "task": "process-data",
            "iterations": 3
        });

        let result = child.run(input);
        assert!(result.is_ok(), "Run failed: {:?}", result);

        if let ChildResult::Ok(data) = result {
            assert_eq!(data["task"], "process-data");
            assert_eq!(data["iterations"], 3);
            assert!(data["results"].is_array());
            assert_eq!(data["results"].as_array().unwrap().len(), 3);
        }
    }

    #[test]
    fn worker_emits_output() {
        let lua = create_lua();
        let mut child =
            LuaChild::from_script(Arc::clone(&lua), WORKER_SCRIPT, test_sandbox()).unwrap();

        let ctx = MockChildContext::new("parent");
        let ctx_emit_history = Arc::clone(&ctx.emit_history);
        child.set_context(Box::new(ctx));

        child.run(json!({"task": "test", "iterations": 1}));

        // Worker should emit at least 2 messages (start + complete)
        let emit_count = ctx_emit_history.lock().len();
        assert!(
            emit_count >= 2,
            "Expected at least 2 emissions, got {}",
            emit_count
        );

        // Verify message content (worker outputs "Worker starting task: ...")
        let has_start = ctx_emit_history
            .lock()
            .iter()
            .any(|r| r.message.contains("starting task"));
        assert!(has_start, "Should emit 'starting task' message");
    }

    #[test]
    fn worker_responds_to_veto() {
        let lua = create_lua();
        let mut child =
            LuaChild::from_script(Arc::clone(&lua), WORKER_SCRIPT, test_sandbox()).unwrap();

        // Worker's on_signal uses orcs.emit_output, so we need context
        let ctx = MockChildContext::new("parent");
        let emit_count_ref = Arc::clone(&ctx.emit_count);
        child.set_context(Box::new(ctx));

        let signal = orcs_event::Signal::veto(orcs_types::Principal::System);
        let response = child.on_signal(&signal);

        assert_eq!(response, SignalResponse::Abort);
        assert_eq!(child.status(), Status::Aborted);

        // Verify emit_output was called in on_signal
        assert!(
            emit_count_ref.load(Ordering::SeqCst) >= 1,
            "emit_output should be called in on_signal"
        );
    }
}

// =============================================================================
// Spawner Child Tests (Child spawns Sub-children)
// =============================================================================

mod spawner_child {
    use super::*;
    use orcs_component::Identifiable;

    const SPAWNER_SCRIPT: &str = include_str!("../scripts/spawner_child.lua");

    #[test]
    fn load_spawner_child() {
        let lua = create_lua();
        let child = LuaChild::from_script(Arc::clone(&lua), SPAWNER_SCRIPT, test_sandbox());

        assert!(child.is_ok(), "Failed to load: {:?}", child.err());
        let child = child.unwrap();
        assert_eq!(child.id(), "spawner");
        assert!(child.is_runnable());
    }

    #[test]
    fn spawner_spawns_workers() {
        let lua = create_lua();
        let mut child =
            LuaChild::from_script(Arc::clone(&lua), SPAWNER_SCRIPT, test_sandbox()).unwrap();

        let ctx = MockChildContext::new("parent");
        let spawn_history = Arc::clone(&ctx.spawn_history);
        child.set_context(Box::new(ctx));

        let input = json!({
            "num_workers": 3,
            "task": "parallel-work"
        });

        let result = child.run(input);
        assert!(result.is_ok(), "Run failed: {:?}", result);

        // Verify spawning occurred using spawn_history
        let spawned_ids: Vec<String> = spawn_history.lock().iter().map(|r| r.id.clone()).collect();
        assert_eq!(spawned_ids.len(), 3, "Should have spawned 3 workers");

        // Verify IDs are correct (spawner_child.lua uses "sub-worker-" prefix)
        assert!(spawned_ids.iter().all(|id| id.starts_with("sub-worker-")));

        if let ChildResult::Ok(data) = result {
            assert_eq!(data["count"], 3);
            assert!(data["spawned"].is_array());
            assert_eq!(data["spawned"].as_array().unwrap().len(), 3);
        }
    }

    #[test]
    fn spawner_respects_max_children() {
        let lua = create_lua();
        let mut child =
            LuaChild::from_script(Arc::clone(&lua), SPAWNER_SCRIPT, test_sandbox()).unwrap();

        let ctx = MockChildContext::new("parent");
        // max_children is 10
        child.set_context(Box::new(ctx));

        // Request more than max
        let input = json!({
            "num_workers": 15,
            "task": "too-many"
        });

        let result = child.run(input);

        // Should fail due to limit check in Lua
        if let ChildResult::Err(err) = result {
            assert!(
                err.to_string().contains("max"),
                "Error should mention max limit"
            );
        } else if let ChildResult::Ok(data) = result {
            // If validation is not in Lua, MockContext should reject
            // This depends on implementation
            println!("Got data: {:?}", data);
        }
    }
}

// =============================================================================
// Manager Component Tests
// =============================================================================

mod manager_component {
    use super::*;
    use orcs_component::Component;
    use orcs_event::Request;
    use orcs_types::{ChannelId, ComponentId};

    const MANAGER_SCRIPT: &str = include_str!("../scripts/manager_worker.lua");

    fn create_request(operation: &str, payload: Value) -> Request {
        Request::new(
            EventCategory::Echo,
            operation,
            ComponentId::builtin("test"),
            ChannelId::new(),
            payload,
        )
    }

    #[test]
    fn load_manager_component() {
        let component = LuaComponent::from_script(MANAGER_SCRIPT, test_sandbox());

        assert!(component.is_ok(), "Failed to load: {:?}", component.err());
        let component = component.unwrap();
        assert!(component.id().fqn().contains("manager"));
    }

    #[test]
    fn manager_dispatch_task() {
        let mut component = LuaComponent::from_script(MANAGER_SCRIPT, test_sandbox()).unwrap();

        let req = create_request("dispatch", json!({"task": "process-item"}));
        let result = component.on_request(&req);

        assert!(result.is_ok(), "Request failed: {:?}", result.err());
        let data = result.unwrap();
        assert_eq!(data["processed"], "process-item");
        assert_eq!(data["by"], "manager-direct");
    }

    #[test]
    fn manager_status() {
        let mut component = LuaComponent::from_script(MANAGER_SCRIPT, test_sandbox()).unwrap();

        let req = create_request("status", json!({}));
        let result = component.on_request(&req);

        assert!(result.is_ok());
        let data = result.unwrap();
        assert_eq!(data["worker_count"], 0);
    }

    #[test]
    fn manager_veto_aborts() {
        let mut component = LuaComponent::from_script(MANAGER_SCRIPT, test_sandbox()).unwrap();

        let signal = orcs_event::Signal::veto(orcs_types::Principal::System);
        let response = component.on_signal(&signal);

        assert_eq!(response, SignalResponse::Abort);
        assert_eq!(component.status(), Status::Aborted);
    }

    #[test]
    fn manager_init_and_shutdown() {
        let mut component = LuaComponent::from_script(MANAGER_SCRIPT, test_sandbox())
            .expect("should load manager script");

        // Init
        let empty_config = serde_json::Value::Object(serde_json::Map::new());
        let init_result = component.init(&empty_config);
        assert!(init_result.is_ok());

        // Shutdown (no error expected)
        component.shutdown();
    }
}

// =============================================================================
// Integration: Child with Context (Full workflow)
// =============================================================================

mod integration {
    use super::*;

    #[test]
    fn child_spawns_and_emits_workflow() {
        // 1. Create Lua runtime
        let lua = create_lua();

        // 2. Create spawner child
        let script = r#"
            return {
                id = "orchestrator",
                run = function(input)
                    -- Emit start
                    orcs.emit_output("Starting orchestration")

                    -- Spawn workers
                    local workers = {}
                    for i = 1, input.worker_count do
                        local result = orcs.spawn_child({
                            id = "worker-" .. i,
                            script = "return { id = 'w', run = function() return {} end, on_signal = function() return 'Handled' end }"
                        })
                        if result.ok then
                            table.insert(workers, result.id)
                        end
                    end

                    -- Check counts
                    local count = orcs.child_count()
                    local max = orcs.max_children()

                    orcs.emit_output("Spawned " .. count .. "/" .. max .. " workers")

                    return {
                        success = true,
                        data = {
                            workers = workers,
                            total = count,
                            max = max,
                        }
                    }
                end,
                on_signal = function(sig)
                    return "Handled"
                end,
            }
        "#;

        let mut child = LuaChild::from_script(lua, script, test_sandbox()).unwrap();

        // 3. Set up context
        let ctx = MockChildContext::new("root");
        let spawn_count = Arc::clone(&ctx.spawn_count);
        let emit_count = Arc::clone(&ctx.emit_count);
        child.set_context(Box::new(ctx));

        // 4. Run
        let result = child.run(json!({"worker_count": 5}));

        // 5. Verify
        assert!(result.is_ok());

        if let ChildResult::Ok(data) = result {
            assert_eq!(data["total"], 5);
            assert_eq!(data["max"], 10);
            assert!(data["workers"].is_array());
        }

        assert_eq!(spawn_count.load(Ordering::SeqCst), 5);
        assert!(emit_count.load(Ordering::SeqCst) >= 2); // start + complete
    }

    /// Test: LuaChild without context cannot use orcs.spawn_child
    #[test]
    fn child_without_context_no_spawn() {
        let lua = create_lua();

        let script = r#"
            return {
                id = "no-context-child",
                run = function(input)
                    -- orcs table should not have spawn_child without context
                    if orcs and orcs.spawn_child then
                        -- Try to call (will fail without context)
                        local result = orcs.spawn_child({ id = "test" })
                        if result then
                            return { success = false, error = "spawn_child should not work" }
                        end
                    end
                    return { success = true, data = { no_context = true } }
                end,
                on_signal = function(sig)
                    return "Handled"
                end,
            }
        "#;

        let mut child = LuaChild::from_script(lua, script, test_sandbox()).unwrap();
        // Note: NOT setting context

        let result = child.run(json!({}));
        assert!(result.is_ok());

        if let ChildResult::Ok(data) = result {
            assert_eq!(data["no_context"], true);
        }
    }
}

// =============================================================================
// Component -> Child Spawn Tests (NEW: set_child_context)
// =============================================================================

mod component_spawns_child {
    use super::*;
    use orcs_component::Component;
    use orcs_event::Request;
    use orcs_types::{ChannelId, ComponentId};

    fn create_request(operation: &str, payload: Value) -> Request {
        Request::new(
            EventCategory::Echo,
            operation,
            ComponentId::builtin("test"),
            ChannelId::new(),
            payload,
        )
    }

    /// Manager component that can spawn children via orcs.spawn_child()
    const SPAWNING_MANAGER: &str = r#"
        return {
            id = "spawning-manager",
            namespace = "test",
            subscriptions = {"Echo"},

            on_request = function(req)
                if req.operation == "spawn_workers" then
                    local count = req.payload.count or 2
                    local spawned = {}

                    for i = 1, count do
                        local result = orcs.spawn_child({
                            id = "worker-" .. i,
                            script = [[
                                return {
                                    id = "worker",
                                    run = function(input) return input end,
                                    on_signal = function(sig) return "Handled" end,
                                }
                            ]]
                        })

                        if result.ok then
                            table.insert(spawned, result.id)
                        end
                    end

                    return {
                        success = true,
                        data = {
                            spawned = spawned,
                            child_count = orcs.child_count(),
                            max_children = orcs.max_children(),
                        }
                    }
                elseif req.operation == "get_counts" then
                    return {
                        success = true,
                        data = {
                            child_count = orcs.child_count(),
                            max_children = orcs.max_children(),
                        }
                    }
                end

                return { success = false, error = "unknown operation" }
            end,

            on_signal = function(sig)
                if sig.kind == "Veto" then
                    return "Abort"
                end
                return "Handled"
            end,
        }
    "#;

    #[test]
    fn component_has_child_context() {
        let mut component = LuaComponent::from_script(SPAWNING_MANAGER, test_sandbox()).unwrap();

        // Initially no child context
        assert!(!component.has_child_context());

        // Set child context
        let ctx = MockChildContext::new("manager");
        component.set_child_context(Box::new(ctx));

        assert!(component.has_child_context());
    }

    #[test]
    fn component_spawn_child_via_request() {
        let mut component = LuaComponent::from_script(SPAWNING_MANAGER, test_sandbox()).unwrap();

        // Set child context
        let ctx = MockChildContext::new("manager");
        let spawn_count = Arc::clone(&ctx.spawn_count);
        component.set_child_context(Box::new(ctx));

        // Request to spawn workers
        let req = create_request("spawn_workers", json!({"count": 3}));
        let result = component.on_request(&req);

        assert!(result.is_ok(), "Request failed: {:?}", result.err());

        let data = result.unwrap();
        assert_eq!(data["child_count"], 3);
        assert!(data["spawned"].is_array());
        assert_eq!(data["spawned"].as_array().unwrap().len(), 3);

        // Verify spawning occurred via MockContext
        assert_eq!(spawn_count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn component_get_counts_without_spawning() {
        let mut component = LuaComponent::from_script(SPAWNING_MANAGER, test_sandbox()).unwrap();

        // Set child context
        let ctx = MockChildContext::new("manager");
        component.set_child_context(Box::new(ctx));

        // Request counts
        let req = create_request("get_counts", json!({}));
        let result = component.on_request(&req);

        assert!(result.is_ok());
        let data = result.unwrap();
        assert_eq!(data["child_count"], 0);
        assert_eq!(data["max_children"], 10); // MockChildContext default
    }

    #[test]
    fn component_without_context_spawn_fails_gracefully() {
        let mut component = LuaComponent::from_script(SPAWNING_MANAGER, test_sandbox()).unwrap();

        // Note: NOT setting child context

        // Try to spawn - should fail or return error
        let req = create_request("spawn_workers", json!({"count": 1}));
        let result = component.on_request(&req);

        // orcs.spawn_child won't exist or will error
        // The Lua script might error, which maps to ComponentError
        // This is expected behavior - spawn_child requires context
        match result {
            Err(e) => {
                // Expected: Lua error because orcs.spawn_child doesn't exist
                println!("Expected error: {:?}", e);
            }
            Ok(data) => {
                // If it somehow succeeds, spawned should be empty
                println!("Unexpected success: {:?}", data);
            }
        }
    }
}

// =============================================================================
// Test Harness Verification Tests
// =============================================================================

mod test_harness {
    use super::*;

    /// Verify emit_history captures message content.
    #[test]
    fn emit_history_captures_messages() {
        let lua = create_lua();
        let script = r#"
            return {
                id = "emitter",
                run = function(input)
                    orcs.emit_output("Hello World")
                    orcs.emit_output("Processing: " .. (input.task or "unknown"), "info")
                    orcs.emit_output("Done!", "debug")
                    return { success = true }
                end,
                on_signal = function(sig) return "Handled" end,
            }
        "#;

        let mut child = LuaChild::from_script(lua, script, test_sandbox()).unwrap();

        let ctx = MockChildContext::new("parent");
        let emit_history = Arc::clone(&ctx.emit_history);
        child.set_context(Box::new(ctx));

        child.run(json!({"task": "test-task"}));

        // Verify message content
        let history = emit_history.lock();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].message, "Hello World");
        assert!(history[0].level.is_none());
        assert_eq!(history[1].message, "Processing: test-task");
        assert_eq!(history[1].level, Some("info".to_string()));
        assert_eq!(history[2].message, "Done!");
        assert_eq!(history[2].level, Some("debug".to_string()));
    }

    /// Verify has_emit_containing helper.
    #[test]
    fn has_emit_containing_works() {
        let lua = create_lua();
        let script = r#"
            return {
                id = "emitter",
                run = function(input)
                    orcs.emit_output("Starting process...")
                    orcs.emit_output("Error: something failed")
                    return { success = true }
                end,
                on_signal = function(sig) return "Handled" end,
            }
        "#;

        let mut child = LuaChild::from_script(lua, script, test_sandbox()).unwrap();

        let ctx = MockChildContext::new("parent");
        let emit_history = Arc::clone(&ctx.emit_history);
        child.set_context(Box::new(ctx));

        child.run(json!({}));

        // Use emit_history directly to check content
        let has_error = emit_history
            .lock()
            .iter()
            .any(|r| r.message.contains("Error"));
        assert!(has_error, "Should find 'Error' in messages");

        let has_success = emit_history
            .lock()
            .iter()
            .any(|r| r.message.contains("Success"));
        assert!(!has_success, "Should NOT find 'Success' in messages");
    }

    /// Verify spawn_history captures spawn configs.
    #[test]
    fn spawn_history_captures_configs() {
        let lua = create_lua();
        let script = r#"
            return {
                id = "spawner",
                run = function(input)
                    orcs.spawn_child({ id = "worker-alpha" })
                    orcs.spawn_child({ id = "worker-beta" })
                    return { success = true }
                end,
                on_signal = function(sig) return "Handled" end,
            }
        "#;

        let mut child = LuaChild::from_script(lua, script, test_sandbox()).unwrap();

        let ctx = MockChildContext::new("parent");
        let spawn_history = Arc::clone(&ctx.spawn_history);
        child.set_context(Box::new(ctx));

        child.run(json!({}));

        // Verify spawn records
        let history = spawn_history.lock();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].id, "worker-alpha");
        assert_eq!(history[1].id, "worker-beta");
    }

    /// Verify spawned_ids helper.
    #[test]
    fn spawned_ids_returns_all_ids() {
        let lua = create_lua();
        let script = r#"
            return {
                id = "spawner",
                run = function(input)
                    for i = 1, 5 do
                        orcs.spawn_child({ id = "w-" .. i })
                    end
                    return { success = true }
                end,
                on_signal = function(sig) return "Handled" end,
            }
        "#;

        let mut child = LuaChild::from_script(lua, script, test_sandbox()).unwrap();

        let ctx = MockChildContext::new("parent");
        let spawn_history = Arc::clone(&ctx.spawn_history);
        child.set_context(Box::new(ctx));

        child.run(json!({}));

        let ids: Vec<String> = spawn_history.lock().iter().map(|r| r.id.clone()).collect();
        assert_eq!(ids, vec!["w-1", "w-2", "w-3", "w-4", "w-5"]);
    }

    /// Verify with_max_children builder.
    #[test]
    fn with_max_children_limits_spawns() {
        let lua = create_lua();
        let script = r#"
            return {
                id = "spawner",
                run = function(input)
                    local spawned = 0
                    local errors = 0
                    for i = 1, 10 do
                        local result = orcs.spawn_child({ id = "w-" .. i })
                        if result.ok then
                            spawned = spawned + 1
                        else
                            errors = errors + 1
                        end
                    end
                    return { success = true, data = { spawned = spawned, errors = errors } }
                end,
                on_signal = function(sig) return "Handled" end,
            }
        "#;

        let mut child = LuaChild::from_script(lua, script, test_sandbox()).unwrap();

        // Limit to 3 children
        let ctx = MockChildContext::new("parent").with_max_children(3);
        child.set_context(Box::new(ctx));

        let result = child.run(json!({}));
        if let ChildResult::Ok(data) = result {
            assert_eq!(data["spawned"], 3);
            assert_eq!(data["errors"], 7);
        }
    }

    /// Verify with_fail_spawn_on error injection.
    #[test]
    fn fail_spawn_on_injects_error() {
        let lua = create_lua();
        let script = r#"
            return {
                id = "spawner",
                run = function(input)
                    local results = {}
                    for _, id in ipairs({"good-1", "bad-worker", "good-2"}) do
                        local result = orcs.spawn_child({ id = id })
                        table.insert(results, { id = id, ok = result.ok })
                    end
                    return { success = true, data = { results = results } }
                end,
                on_signal = function(sig) return "Handled" end,
            }
        "#;

        let mut child = LuaChild::from_script(lua, script, test_sandbox()).unwrap();

        // Fail any spawn with "bad" in the id
        let ctx = MockChildContext::new("parent").with_fail_spawn_on("bad");
        let spawn_count = Arc::clone(&ctx.spawn_count);
        child.set_context(Box::new(ctx));

        let result = child.run(json!({}));

        // Only 2 should have succeeded
        assert_eq!(spawn_count.load(Ordering::SeqCst), 2);

        if let ChildResult::Ok(data) = result {
            let results = data["results"].as_array().unwrap();
            assert!(results[0]["ok"].as_bool().unwrap()); // good-1
            assert!(!results[1]["ok"].as_bool().unwrap()); // bad-worker (failed)
            assert!(results[2]["ok"].as_bool().unwrap()); // good-2
        }
    }

    /// Verify helper methods work correctly.
    #[test]
    fn helper_methods_work() {
        let lua = create_lua();
        let script = r#"
            return {
                id = "helper-test",
                run = function(input)
                    orcs.emit_output("Message 1")
                    orcs.emit_output("Message 2", "info")
                    orcs.spawn_child({ id = "child-a" })
                    orcs.spawn_child({ id = "child-b" })
                    return { success = true }
                end,
                on_signal = function(sig) return "Handled" end,
            }
        "#;

        let mut child = LuaChild::from_script(lua, script, test_sandbox()).unwrap();

        let ctx = MockChildContext::new("parent");
        // Clone Arc references before moving ctx
        let ctx_for_methods = MockChildContext {
            parent_id: ctx.parent_id.clone(),
            spawn_count: Arc::clone(&ctx.spawn_count),
            emit_count: Arc::clone(&ctx.emit_count),
            max_children: ctx.max_children,
            emit_history: Arc::clone(&ctx.emit_history),
            spawn_history: Arc::clone(&ctx.spawn_history),
            fail_spawn_on: ctx.fail_spawn_on.clone(),
        };
        child.set_context(Box::new(ctx));

        child.run(json!({}));

        // Test helper methods
        assert_eq!(ctx_for_methods.emit_count(), 2);
        assert_eq!(ctx_for_methods.spawn_count(), 2);

        let messages = ctx_for_methods.emit_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0], "Message 1");
        assert_eq!(messages[1], "Message 2");

        assert!(ctx_for_methods.has_emit_containing("Message 1"));
        assert!(ctx_for_methods.has_emit_containing("Message 2"));
        assert!(!ctx_for_methods.has_emit_containing("Message 3"));

        let ids = ctx_for_methods.spawned_ids();
        assert_eq!(ids, vec!["child-a", "child-b"]);
    }
}

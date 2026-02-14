//! Integration tests for the Life Game Lua component.
//!
//! Tests the 1D cellular automaton (ring topology) with:
//! - Cell worker spawning via ChildContext
//! - Parallel batch execution via send_to_children_batch
//! - Grid rendering and simulation history
//!
//! The MockCellContext simulates cell worker behavior by extracting the
//! `me` field from input, applying the 1D automaton rules, and returning
//! the result. This allows testing the full simulation flow without
//! actual child processes.

use orcs_component::{
    ChildConfig, ChildContext, ChildHandle, ChildResult, Component, RunError, SpawnError, Status,
};
use orcs_event::{EventCategory, Request, Signal, SignalResponse};
use orcs_lua::LuaComponent;
use orcs_runtime::sandbox::{ProjectSandbox, SandboxPolicy};
use orcs_types::{ChannelId, ComponentId, Principal};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const LIFE_GAME_SCRIPT: &str = include_str!("../../orcs-app/builtins/life_game.lua");

fn test_sandbox() -> Arc<dyn SandboxPolicy> {
    Arc::new(ProjectSandbox::new(".").expect("test sandbox"))
}

fn create_request(operation: &str, payload: Value) -> Request {
    Request::new(
        EventCategory::Echo,
        operation,
        ComponentId::builtin("test"),
        ChannelId::new(),
        payload,
    )
}

// =============================================================================
// Mock Cell Context
// =============================================================================

/// Mock ChildContext that simulates cell worker behavior.
///
/// `send_to_child` applies the 1D automaton rules to the input,
/// matching what the real cell worker Lua script would compute.
#[derive(Debug)]
struct MockCellContext {
    parent_id: String,
    spawn_count: Arc<AtomicUsize>,
    max_children: usize,
    /// Track spawned IDs for verification.
    spawn_history: Arc<Mutex<Vec<String>>>,
}

impl MockCellContext {
    fn new(parent_id: &str) -> Self {
        Self {
            parent_id: parent_id.into(),
            spawn_count: Arc::new(AtomicUsize::new(0)),
            max_children: 64,
            spawn_history: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_max_children(mut self, max: usize) -> Self {
        self.max_children = max;
        self
    }
}

/// Mock handle returned by spawn_child.
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
    fn run_sync(&mut self, _input: Value) -> Result<ChildResult, RunError> {
        Ok(ChildResult::Ok(Value::Null))
    }
    fn abort(&mut self) {}
    fn is_finished(&self) -> bool {
        false
    }
}

impl ChildContext for MockCellContext {
    fn parent_id(&self) -> &str {
        &self.parent_id
    }

    fn emit_output(&self, _message: &str) {
        // noop for tests (output goes to tracing)
    }

    fn emit_output_with_level(&self, _message: &str, _level: &str) {
        // noop for tests
    }

    fn spawn_child(&self, config: ChildConfig) -> Result<Box<dyn ChildHandle>, SpawnError> {
        if self.spawn_count.load(Ordering::SeqCst) >= self.max_children {
            return Err(SpawnError::MaxChildrenReached(self.max_children));
        }
        self.spawn_count.fetch_add(1, Ordering::SeqCst);
        self.spawn_history
            .lock()
            .expect("spawn_history lock")
            .push(config.id.clone());
        Ok(Box::new(MockHandle { id: config.id }))
    }

    fn child_count(&self) -> usize {
        self.spawn_count.load(Ordering::SeqCst)
    }

    fn max_children(&self) -> usize {
        self.max_children
    }

    fn send_to_child(&self, _child_id: &str, input: Value) -> Result<ChildResult, RunError> {
        // Simulate cell worker logic:
        // Input: { me: {alive, kind}, left: {alive, kind}, right: {alive, kind} }
        // Output: { alive: bool, kind: string }
        let me = &input["me"];
        let left = &input["left"];
        let right = &input["right"];

        let me_alive = me["alive"].as_bool().unwrap_or(false);
        let me_kind = me["kind"].as_str().unwrap_or("R").to_string();
        let left_alive = left["alive"].as_bool().unwrap_or(false);
        let left_kind = left["kind"].as_str().unwrap_or("R").to_string();
        let right_alive = right["alive"].as_bool().unwrap_or(false);
        let right_kind = right["kind"].as_str().unwrap_or("R").to_string();

        let n = (if left_alive { 1 } else { 0 }) + (if right_alive { 1 } else { 0 });

        let (alive, kind) = if me_alive {
            if n == 0 {
                (false, me_kind)
            } else if n == 2 && left_kind != right_kind {
                let new_kind = if me_kind == left_kind {
                    right_kind
                } else if me_kind == right_kind {
                    left_kind
                } else {
                    me_kind
                };
                (true, new_kind)
            } else {
                (true, me_kind)
            }
        } else if n >= 1 {
            let birth_kind = if left_alive { left_kind } else { right_kind };
            (true, birth_kind)
        } else {
            (false, me_kind)
        };

        Ok(ChildResult::Ok(json!({ "alive": alive, "kind": kind })))
    }

    fn clone_box(&self) -> Box<dyn ChildContext> {
        Box::new(Self {
            parent_id: self.parent_id.clone(),
            spawn_count: Arc::clone(&self.spawn_count),
            max_children: self.max_children,
            spawn_history: Arc::clone(&self.spawn_history),
        })
    }
}

// =============================================================================
// Component Loading Tests
// =============================================================================

#[test]
fn load_life_game_component() {
    let component = LuaComponent::from_script(LIFE_GAME_SCRIPT, test_sandbox());

    assert!(
        component.is_ok(),
        "Failed to load life_game.lua: {:?}",
        component.err()
    );
    let component = component.expect("load life_game");
    assert!(
        component.id().fqn().contains("life_game"),
        "Component FQN should contain 'life_game', got: {}",
        component.id().fqn()
    );
}

#[test]
fn life_game_init_and_shutdown() {
    let mut component =
        LuaComponent::from_script(LIFE_GAME_SCRIPT, test_sandbox()).expect("load life_game");

    let init_result = component.init();
    assert!(
        init_result.is_ok(),
        "init() failed: {:?}",
        init_result.err()
    );

    // shutdown should not panic
    component.shutdown();
}

#[test]
fn life_game_veto_aborts() {
    let mut component =
        LuaComponent::from_script(LIFE_GAME_SCRIPT, test_sandbox()).expect("load life_game");

    let signal = Signal::veto(Principal::System);
    let response = component.on_signal(&signal);

    assert_eq!(response, SignalResponse::Abort);
    assert_eq!(component.status(), Status::Aborted);
}

// =============================================================================
// Run Operation Tests
// =============================================================================

#[test]
fn life_game_run_default_params() {
    let mut component =
        LuaComponent::from_script(LIFE_GAME_SCRIPT, test_sandbox()).expect("load life_game");

    let ctx = MockCellContext::new("life_game");
    let spawn_count = Arc::clone(&ctx.spawn_count);
    component.set_child_context(Box::new(ctx));

    let req = create_request("run", json!({}));
    let result = component.on_request(&req);

    assert!(result.is_ok(), "run failed: {:?}", result.err());
    let data = result.expect("run result");

    // Default: grid_size=16, generations=3
    assert_eq!(data["grid_size"], 16, "default grid_size should be 16");
    assert_eq!(data["generations"], 3, "default generations should be 3");
    assert!(data["alive"].is_number(), "alive should be a number");
    assert!(
        data["kind_counts"].is_object(),
        "kind_counts should be an object"
    );

    // History should have gen0 + 3 generations = 4 entries
    let history = data["history"]
        .as_array()
        .expect("history should be an array");
    assert_eq!(
        history.len(),
        4,
        "history should have 4 entries (gen0 + 3 gens)"
    );

    // Verify spawning: 16 cells should be spawned
    assert_eq!(
        spawn_count.load(Ordering::SeqCst),
        16,
        "should spawn 16 cell workers"
    );
}

#[test]
fn life_game_run_custom_params() {
    let mut component =
        LuaComponent::from_script(LIFE_GAME_SCRIPT, test_sandbox()).expect("load life_game");

    let ctx = MockCellContext::new("life_game");
    let spawn_count = Arc::clone(&ctx.spawn_count);
    let spawn_history = Arc::clone(&ctx.spawn_history);
    component.set_child_context(Box::new(ctx));

    let req = create_request(
        "run",
        json!({
            "grid_size": 8,
            "generations": 2,
        }),
    );
    let result = component.on_request(&req);

    assert!(result.is_ok(), "run failed: {:?}", result.err());
    let data = result.expect("run result");

    assert_eq!(data["grid_size"], 8);
    assert_eq!(data["generations"], 2);

    // History: gen0 + 2 generations = 3
    let history = data["history"]
        .as_array()
        .expect("history should be an array");
    assert_eq!(history.len(), 3);

    // 8 cell workers spawned
    assert_eq!(spawn_count.load(Ordering::SeqCst), 8);

    // Verify cell IDs
    let ids = spawn_history.lock().expect("spawn_history lock");
    assert_eq!(ids.len(), 8);
    assert_eq!(ids[0], "cell-1");
    assert_eq!(ids[7], "cell-8");
}

#[test]
fn life_game_run_verifies_grid_rendering() {
    let mut component =
        LuaComponent::from_script(LIFE_GAME_SCRIPT, test_sandbox()).expect("load life_game");

    let ctx = MockCellContext::new("life_game");
    component.set_child_context(Box::new(ctx));

    let req = create_request(
        "run",
        json!({
            "grid_size": 6,
            "generations": 1,
        }),
    );
    let result = component.on_request(&req);

    assert!(result.is_ok(), "run failed: {:?}", result.err());
    let data = result.expect("run result");

    // Default initial state: every 3rd cell dead, kinds cycle R/G/B
    // Grid: [R, G, ., R, G, .]  (alive, alive, dead, alive, alive, dead)
    // Gen 0 rendering should be "RG.RG."
    let history = data["history"]
        .as_array()
        .expect("history should be an array");
    assert_eq!(
        history[0].as_str().expect("gen0 should be string"),
        "RG.RG.",
        "Gen 0 should match default initial state"
    );
}

#[test]
fn life_game_run_simulation_produces_changes() {
    let mut component =
        LuaComponent::from_script(LIFE_GAME_SCRIPT, test_sandbox()).expect("load life_game");

    let ctx = MockCellContext::new("life_game");
    component.set_child_context(Box::new(ctx));

    let req = create_request(
        "run",
        json!({
            "grid_size": 6,
            "generations": 2,
        }),
    );
    let result = component.on_request(&req);

    assert!(result.is_ok(), "run failed: {:?}", result.err());
    let data = result.expect("run result");

    let history = data["history"]
        .as_array()
        .expect("history should be an array");

    // Gen 0: "RG.RG."
    assert_eq!(history[0].as_str().expect("gen0"), "RG.RG.");

    // After gen 1, dead cells with alive neighbors should be born.
    // The mock implements the actual rules, so the grid should change.
    let gen1 = history[1].as_str().expect("gen1 should be string");
    assert_ne!(
        gen1, "RG.RG.",
        "Gen 1 should differ from Gen 0 (dead cells birth)"
    );

    // All history entries should be 6 characters (grid_size)
    for (i, entry) in history.iter().enumerate() {
        let s = entry.as_str().unwrap_or("");
        assert_eq!(
            s.len(),
            6,
            "History entry {} should have 6 chars, got: '{}'",
            i,
            s
        );
    }
}

// =============================================================================
// Error Cases
// =============================================================================

#[test]
fn life_game_grid_size_exceeds_max() {
    let mut component =
        LuaComponent::from_script(LIFE_GAME_SCRIPT, test_sandbox()).expect("load life_game");

    let ctx = MockCellContext::new("life_game");
    component.set_child_context(Box::new(ctx));

    let req = create_request("run", json!({"grid_size": 100}));
    let result = component.on_request(&req);

    // Should return error (success=false), which maps to ComponentError
    assert!(result.is_err(), "grid_size > 64 should fail");
    let err = result.expect_err("should be error");
    let err_msg = err.to_string();
    assert!(
        err_msg.contains("max") || err_msg.contains("64"),
        "error should mention max limit, got: {}",
        err_msg
    );
}

#[test]
fn life_game_unknown_operation() {
    let mut component =
        LuaComponent::from_script(LIFE_GAME_SCRIPT, test_sandbox()).expect("load life_game");

    let ctx = MockCellContext::new("life_game");
    component.set_child_context(Box::new(ctx));

    let req = create_request("nonexistent", json!({}));
    let result = component.on_request(&req);

    assert!(result.is_err(), "unknown operation should fail");
    let err = result.expect_err("should be error");
    assert!(
        err.to_string().contains("unknown operation"),
        "error should mention unknown operation, got: {}",
        err
    );
}

#[test]
fn life_game_spawn_failure_returns_error() {
    let mut component =
        LuaComponent::from_script(LIFE_GAME_SCRIPT, test_sandbox()).expect("load life_game");

    // max_children=0 → spawn always fails
    let ctx = MockCellContext::new("life_game").with_max_children(0);
    component.set_child_context(Box::new(ctx));

    let req = create_request("run", json!({"grid_size": 4, "generations": 1}));
    let result = component.on_request(&req);

    assert!(
        result.is_err(),
        "should fail when cell spawning is impossible"
    );
}

// =============================================================================
// Status Operation Tests
// =============================================================================

#[test]
fn life_game_status_before_run() {
    let mut component =
        LuaComponent::from_script(LIFE_GAME_SCRIPT, test_sandbox()).expect("load life_game");

    let ctx = MockCellContext::new("life_game");
    component.set_child_context(Box::new(ctx));

    let req = create_request("status", json!({}));
    let result = component.on_request(&req);

    assert!(result.is_ok(), "status failed: {:?}", result.err());
    let data = result.expect("status result");

    // Before any run, grid is empty
    assert_eq!(data["grid_size"], 0, "grid_size should be 0 before run");
    assert_eq!(data["alive"], 0, "alive should be 0 before run");
}

#[test]
fn life_game_status_after_run() {
    let mut component =
        LuaComponent::from_script(LIFE_GAME_SCRIPT, test_sandbox()).expect("load life_game");

    let ctx = MockCellContext::new("life_game");
    component.set_child_context(Box::new(ctx));

    // Run first
    let run_req = create_request("run", json!({"grid_size": 6, "generations": 1}));
    let run_result = component.on_request(&run_req);
    assert!(run_result.is_ok(), "run failed: {:?}", run_result.err());

    // Then check status
    let status_req = create_request("status", json!({}));
    let status_result = component.on_request(&status_req);

    assert!(
        status_result.is_ok(),
        "status failed: {:?}",
        status_result.err()
    );
    let data = status_result.expect("status result");

    assert_eq!(data["grid_size"], 6, "grid_size should be 6 after run");
    assert!(
        data["alive"].as_u64().expect("alive should be number") > 0,
        "some cells should be alive"
    );
    assert!(
        data["grid"].as_str().expect("grid should be string").len() == 6,
        "grid rendering should be 6 chars"
    );
}

// =============================================================================
// Incremental Spawning (cells reused across runs)
// =============================================================================

#[test]
fn life_game_reuses_spawned_cells() {
    let mut component =
        LuaComponent::from_script(LIFE_GAME_SCRIPT, test_sandbox()).expect("load life_game");

    let ctx = MockCellContext::new("life_game");
    let spawn_count = Arc::clone(&ctx.spawn_count);
    component.set_child_context(Box::new(ctx));

    // First run: 4 cells
    let req1 = create_request("run", json!({"grid_size": 4, "generations": 1}));
    let r1 = component.on_request(&req1);
    assert!(r1.is_ok(), "first run failed: {:?}", r1.err());
    assert_eq!(spawn_count.load(Ordering::SeqCst), 4, "first run spawns 4");

    // Second run: same size → no new spawns
    let req2 = create_request("run", json!({"grid_size": 4, "generations": 1}));
    let r2 = component.on_request(&req2);
    assert!(r2.is_ok(), "second run failed: {:?}", r2.err());
    assert_eq!(
        spawn_count.load(Ordering::SeqCst),
        4,
        "second run should not spawn new cells"
    );

    // Third run: larger size → spawns additional cells
    let req3 = create_request("run", json!({"grid_size": 6, "generations": 1}));
    let r3 = component.on_request(&req3);
    assert!(r3.is_ok(), "third run failed: {:?}", r3.err());
    assert_eq!(
        spawn_count.load(Ordering::SeqCst),
        6,
        "third run should spawn 2 more cells"
    );
}

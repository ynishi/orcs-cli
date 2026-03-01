//! Integration tests for agent_mgr supervisor patterns.
//!
//! Tests the runner_exited handler logic (concierge/delegate-worker restart,
//! external concierge protection, restart intensity, Lifecycle fallthrough guard)
//! using a self-contained Lua script that replicates the supervisor patterns
//! from `agent_mgr.lua` without the full dependency chain (orcs.llm, orcs.spawn_runner, etc.).

use orcs_component::EventCategory;
use orcs_lua::testing::LuaTestHarness;
use orcs_runtime::sandbox::{ProjectSandbox, SandboxPolicy};
use serde_json::json;
use std::sync::Arc;

fn test_policy() -> Arc<dyn SandboxPolicy> {
    Arc::new(ProjectSandbox::new(".").expect("test sandbox"))
}

/// Self-contained supervisor test component.
///
/// Replicates the runner_exited handler, restart intensity logic, and
/// Lifecycle fallthrough guard from `agent_mgr.lua`.
///
/// Mock: `orcs.spawn_runner()` is replaced by local state update.
/// The restart result is deterministic (always succeeds unless intensity exceeded).
const SUPERVISOR_SCRIPT: &str = r###"
-- Managed FQNs (mirrors agent_mgr.lua's runtime state)
local concierge_fqn = "builtin::concierge-v1"
local delegate_worker_fqn = "builtin::delegate-worker-v1"

-- Restart intensity tracking (Erlang-inspired: max N restarts in T seconds)
local restart_counts = {}
local MAX_RESTARTS = 3
local RESTART_WINDOW_SECS = 60

-- Test observability: restart log and delegation results
local restart_log = {}
local delegation_results = {}
local DELEGATION_RESULTS_LIMIT = 50

-- Mock: allow tests to override os.time() for window testing
local mock_time = nil

local function now()
    return mock_time or os.time()
end

local function check_restart_allowed(fqn)
    local t = now()
    local entry = restart_counts[fqn]
    if not entry or (t - entry.window_start) > RESTART_WINDOW_SECS then
        restart_counts[fqn] = { count = 1, window_start = t }
        return true
    end
    entry.count = entry.count + 1
    if entry.count > MAX_RESTARTS then
        return false
    end
    return true
end

--- Mock spawn: always succeeds. Returns new FQN with version bump.
local spawn_counter = 0
local function attempt_restart_concierge(old_fqn)
    local check_fqn = old_fqn or concierge_fqn or "builtin::concierge"
    if not check_restart_allowed(check_fqn) then
        restart_log[#restart_log + 1] = {
            target = "concierge", fqn = check_fqn,
            success = false, reason = "intensity_exceeded",
        }
        return false
    end
    spawn_counter = spawn_counter + 1
    concierge_fqn = "builtin::concierge-v" .. (spawn_counter + 1)
    restart_log[#restart_log + 1] = {
        target = "concierge", fqn = concierge_fqn,
        success = true,
    }
    return true
end

local function attempt_restart_delegate_worker(old_fqn)
    local check_fqn = old_fqn or delegate_worker_fqn or "builtin::delegate-worker"
    if not check_restart_allowed(check_fqn) then
        restart_log[#restart_log + 1] = {
            target = "delegate_worker", fqn = check_fqn,
            success = false, reason = "intensity_exceeded",
        }
        return false
    end
    spawn_counter = spawn_counter + 1
    delegate_worker_fqn = "builtin::delegate-worker-v" .. (spawn_counter + 1)
    restart_log[#restart_log + 1] = {
        target = "delegate_worker", fqn = delegate_worker_fqn,
        success = true,
    }
    return true
end

return {
    id = "supervisor-test",
    subscriptions = {"Lifecycle", "UserInput"},

    on_request = function(req)
        local op = req.operation
        local payload = req.payload or {}

        -- Test utility: set mock time for restart window tests.
        if op == "_set_mock_time" then
            mock_time = payload.time
            return { success = true }
        end

        -- Test utility: force-set restart_counts for intensity edge cases.
        if op == "_set_restart_count" then
            restart_counts[payload.fqn] = {
                count = payload.count,
                window_start = payload.window_start or now(),
            }
            return { success = true }
        end

        -- Test utility: replace managed FQNs (e.g. to test external concierge).
        if op == "_set_fqn" then
            if payload.concierge ~= nil then concierge_fqn = payload.concierge end
            if payload.delegate_worker ~= nil then delegate_worker_fqn = payload.delegate_worker end
            return { success = true }
        end

        -- Status: return full internal state for assertions.
        if op == "status" then
            return {
                success = true,
                data = {
                    concierge_fqn = concierge_fqn,
                    delegate_worker_fqn = delegate_worker_fqn,
                    restart_log = restart_log,
                    delegation_results = delegation_results,
                },
            }
        end

        -- Handler: Lifecycle::runner_exited (mirrors agent_mgr.lua)
        if op == "runner_exited" then
            local fqn = payload.component_fqn or "unknown"
            local reason = payload.exit_reason or "unknown"

            -- Concierge died
            if fqn == concierge_fqn then
                local old_fqn = concierge_fqn
                concierge_fqn = nil

                if old_fqn:match("^builtin::") then
                    local ok = attempt_restart_concierge(old_fqn)
                    return { success = true, data = { action = "restart_concierge", restarted = ok } }
                else
                    return { success = true, data = { action = "external_concierge_exited", old_fqn = old_fqn } }
                end

            -- Persistent delegate-worker died (checked BEFORE per-delegation pattern)
            elseif fqn == delegate_worker_fqn then
                local old_fqn = delegate_worker_fqn
                delegate_worker_fqn = nil
                local ok = attempt_restart_delegate_worker(old_fqn)
                return { success = true, data = { action = "restart_delegate_worker", restarted = ok } }

            -- Per-delegation worker died
            elseif fqn and fqn:match("^builtin::delegate%-") then
                local request_id = fqn:match("^builtin::delegate%-(.+)$")
                if request_id then
                    delegation_results[#delegation_results + 1] = {
                        request_id = request_id,
                        summary = "Worker exited: " .. reason,
                        success = false,
                    }
                    while #delegation_results > DELEGATION_RESULTS_LIMIT do
                        table.remove(delegation_results, 1)
                    end
                    return { success = true, data = { action = "delegation_failed", request_id = request_id } }
                end
            end

            -- Unmanaged component
            return { success = true, data = { action = "unmanaged", fqn = fqn } }
        end

        -- Lifecycle fallthrough guard: unknown Lifecycle ops → success.
        if req.category == "Lifecycle" then
            return { success = true, data = { action = "lifecycle_ignored" } }
        end

        -- UserInput handler (minimal, just to verify fallthrough guard)
        if req.category == "UserInput" then
            if not payload.message or payload.message == "" then
                return { success = false, error = "empty message" }
            end
            return { success = true, data = { echo = payload.message } }
        end

        return { success = false, error = "unknown operation: " .. tostring(op) }
    end,

    on_signal = function(signal)
        if signal.kind == "Veto" then return "Abort" end
        return "Handled"
    end,
}
"###;

// ─── Helper ─────────────────────────────────────────────────────────

fn make_harness() -> LuaTestHarness {
    LuaTestHarness::from_script(SUPERVISOR_SCRIPT, test_policy())
        .expect("should create supervisor test harness")
}

fn lifecycle() -> EventCategory {
    EventCategory::Lifecycle
}

fn user_input() -> EventCategory {
    EventCategory::UserInput
}

fn send_runner_exited(h: &mut LuaTestHarness, fqn: &str, exit_reason: &str) -> serde_json::Value {
    h.request(
        lifecycle(),
        "runner_exited",
        json!({
            "component_fqn": fqn,
            "exit_reason": exit_reason,
        }),
    )
    .expect("runner_exited request should succeed")
}

fn get_status(h: &mut LuaTestHarness) -> serde_json::Value {
    h.request(lifecycle(), "status", json!({}))
        .expect("status request should succeed")
}

// ─── Concierge restart ──────────────────────────────────────────────

#[test]
fn concierge_restart_on_runner_exited() {
    let mut h = make_harness();

    let result = send_runner_exited(&mut h, "builtin::concierge-v1", "component_stopped");

    assert_eq!(
        result["action"], "restart_concierge",
        "should trigger concierge restart"
    );
    assert_eq!(result["restarted"], true, "restart should succeed");

    // Verify new FQN was assigned
    let status = get_status(&mut h);
    let new_fqn = status["concierge_fqn"]
        .as_str()
        .expect("concierge_fqn should be a string");
    assert!(
        new_fqn.starts_with("builtin::concierge-v"),
        "new concierge FQN should have version bump: {new_fqn}"
    );
    assert_ne!(
        new_fqn, "builtin::concierge-v1",
        "new FQN should differ from original"
    );
}

// ─── Delegate-worker restart ────────────────────────────────────────

#[test]
fn delegate_worker_restart_on_runner_exited() {
    let mut h = make_harness();

    let result = send_runner_exited(&mut h, "builtin::delegate-worker-v1", "component_stopped");

    assert_eq!(
        result["action"], "restart_delegate_worker",
        "should trigger delegate-worker restart"
    );
    assert_eq!(result["restarted"], true, "restart should succeed");

    let status = get_status(&mut h);
    let new_fqn = status["delegate_worker_fqn"]
        .as_str()
        .expect("delegate_worker_fqn should be a string");
    assert!(
        new_fqn.starts_with("builtin::delegate-worker-v"),
        "new delegate-worker FQN should have version bump: {new_fqn}"
    );
}

// ─── External concierge protection ─────────────────────────────────

#[test]
fn external_concierge_not_replaced_with_builtin() {
    let mut h = make_harness();

    // Replace concierge FQN with external
    h.request(
        lifecycle(),
        "_set_fqn",
        json!({ "concierge": "custom::external-concierge" }),
    )
    .expect("set FQN should succeed");

    let result = send_runner_exited(&mut h, "custom::external-concierge", "component_stopped");

    assert_eq!(
        result["action"], "external_concierge_exited",
        "should NOT restart external concierge"
    );
    assert_eq!(
        result["old_fqn"], "custom::external-concierge",
        "should report the external FQN"
    );

    // concierge_fqn should be nil (cleared, not replaced)
    let status = get_status(&mut h);
    assert!(
        status["concierge_fqn"].is_null(),
        "concierge_fqn should be nil after external exit"
    );
}

// ─── Restart intensity limit ────────────────────────────────────────

#[test]
fn restart_intensity_blocks_after_max_restarts() {
    let mut h = make_harness();

    // Restart 1-3: should all succeed
    for i in 1..=3 {
        // Reset FQN to the "current" concierge so the match works
        let status = get_status(&mut h);
        let current_fqn = if status["concierge_fqn"].is_null() {
            // After a restart, FQN was updated. Set it to match next time.
            // But the restart_counts track the old FQN. Simulate fresh crash.
            // For simplicity, use _set_fqn to set a known FQN each round.
            let fqn = format!("builtin::concierge-round-{i}");
            h.request(lifecycle(), "_set_fqn", json!({ "concierge": fqn }))
                .expect("set FQN should succeed");
            fqn
        } else {
            status["concierge_fqn"]
                .as_str()
                .expect("fqn string")
                .to_string()
        };

        let result = send_runner_exited(&mut h, &current_fqn, "component_stopped");
        assert_eq!(result["restarted"], true, "restart #{i} should succeed");
    }

    // Restart 4: set the FQN to the same lineage to hit intensity limit.
    // The restart_counts track by check_fqn. After 3 restarts of the same
    // FQN within the window, the 4th should be rejected.
    //
    // Force count to 3 for a known FQN, then attempt one more.
    h.request(
        lifecycle(),
        "_set_restart_count",
        json!({ "fqn": "builtin::concierge-intensity", "count": 3 }),
    )
    .expect("set restart count should succeed");
    h.request(
        lifecycle(),
        "_set_fqn",
        json!({ "concierge": "builtin::concierge-intensity" }),
    )
    .expect("set FQN should succeed");

    let result = send_runner_exited(&mut h, "builtin::concierge-intensity", "component_stopped");
    assert_eq!(
        result["restarted"], false,
        "4th restart should be blocked by intensity limit"
    );

    // Verify restart log records the failure
    let status = get_status(&mut h);
    let log = status["restart_log"]
        .as_array()
        .expect("restart_log should be an array");
    let last = log.last().expect("restart_log should not be empty");
    assert_eq!(last["success"], false);
    assert_eq!(last["reason"], "intensity_exceeded");
}

// ─── Restart window reset ───────────────────────────────────────────

#[test]
fn restart_window_resets_after_expiry() {
    let mut h = make_harness();

    let base_time = 1_000_000;

    // Set mock time and exhaust restart budget
    h.request(lifecycle(), "_set_mock_time", json!({ "time": base_time }))
        .expect("set mock time should succeed");
    h.request(
        lifecycle(),
        "_set_restart_count",
        json!({ "fqn": "builtin::concierge-window", "count": 3, "window_start": base_time }),
    )
    .expect("set restart count should succeed");
    h.request(
        lifecycle(),
        "_set_fqn",
        json!({ "concierge": "builtin::concierge-window" }),
    )
    .expect("set FQN should succeed");

    // Attempt restart within window → should fail
    let result = send_runner_exited(&mut h, "builtin::concierge-window", "component_stopped");
    assert_eq!(result["restarted"], false, "should fail within window");

    // Advance time past the window (61 seconds)
    h.request(
        lifecycle(),
        "_set_mock_time",
        json!({ "time": base_time + 61 }),
    )
    .expect("advance mock time should succeed");

    // Set FQN again (was cleared by previous attempt)
    h.request(
        lifecycle(),
        "_set_fqn",
        json!({ "concierge": "builtin::concierge-window" }),
    )
    .expect("set FQN should succeed");

    let result = send_runner_exited(&mut h, "builtin::concierge-window", "component_stopped");
    assert_eq!(
        result["restarted"], true,
        "should succeed after window expiry"
    );
}

// ─── Per-delegation worker failure ──────────────────────────────────

#[test]
fn per_delegation_worker_failure_recorded() {
    let mut h = make_harness();

    let result = send_runner_exited(&mut h, "builtin::delegate-req-abc123", "component_stopped");

    assert_eq!(result["action"], "delegation_failed");
    assert_eq!(result["request_id"], "req-abc123");

    let status = get_status(&mut h);
    let results = status["delegation_results"]
        .as_array()
        .expect("delegation_results should be an array");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["request_id"], "req-abc123");
    assert_eq!(results[0]["success"], false);
    assert!(
        results[0]["summary"]
            .as_str()
            .expect("summary string")
            .contains("component_stopped"),
        "summary should contain the exit reason"
    );
}

// ─── Delegate-worker FQN priority ───────────────────────────────────

/// "builtin::delegate-worker" matches delegate_worker_fqn BEFORE the
/// per-delegation pattern "^builtin::delegate%-". Without the correct
/// branch order, it would be misidentified as request_id="worker".
#[test]
fn delegate_worker_fqn_matches_before_per_delegation_pattern() {
    let mut h = make_harness();

    // The initial delegate_worker_fqn is "builtin::delegate-worker-v1"
    // which also matches "^builtin::delegate%-". Verify the exact match wins.
    let result = send_runner_exited(&mut h, "builtin::delegate-worker-v1", "signal");

    assert_eq!(
        result["action"], "restart_delegate_worker",
        "exact delegate_worker_fqn match should take priority"
    );
    // Must NOT be "delegation_failed"
    assert_ne!(
        result["action"].as_str().unwrap_or(""),
        "delegation_failed",
        "should NOT fall through to per-delegation pattern"
    );
}

// ─── Lifecycle fallthrough guard ────────────────────────────────────

#[test]
fn unknown_lifecycle_op_does_not_reach_userinput_handler() {
    let mut h = make_harness();

    let result = h
        .request(
            lifecycle(),
            "some_future_lifecycle_event",
            json!({ "data": "whatever" }),
        )
        .expect("unknown lifecycle op should succeed");

    assert_eq!(
        result["action"], "lifecycle_ignored",
        "unknown Lifecycle op should be caught by fallthrough guard"
    );

    // Verify it did NOT fall through to UserInput handler (which would error on empty message)
    assert!(
        result.get("error").is_none(),
        "should not produce an error (would indicate UserInput fallthrough)"
    );
}

// ─── Unmanaged component ────────────────────────────────────────────

#[test]
fn unmanaged_component_exit_is_logged_only() {
    let mut h = make_harness();

    let result = send_runner_exited(&mut h, "custom::some-other-component", "channel_inactive");

    assert_eq!(
        result["action"], "unmanaged",
        "unmanaged component should just be logged"
    );
    assert_eq!(result["fqn"], "custom::some-other-component");

    // No restarts should have been attempted
    let status = get_status(&mut h);
    let log = &status["restart_log"];
    // Empty Lua table serialises as JSON object {}, not array [].
    let is_empty = log.is_null()
        || log.as_array().map_or(false, |a| a.is_empty())
        || log.as_object().map_or(false, |o| o.is_empty());
    assert!(is_empty, "no restarts for unmanaged components");
}

// ─── UserInput NOT affected by Lifecycle handling ───────────────────

#[test]
fn userinput_still_works_after_lifecycle_events() {
    let mut h = make_harness();

    // Process a Lifecycle event first
    send_runner_exited(&mut h, "custom::something", "signal");

    // UserInput should still work normally
    let result = h
        .request(user_input(), "input", json!({ "message": "hello" }))
        .expect("UserInput should succeed");
    assert_eq!(result["echo"], "hello");
}

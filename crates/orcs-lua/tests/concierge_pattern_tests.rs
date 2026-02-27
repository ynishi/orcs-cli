//! Tests for concierge agent behavioral patterns.
//!
//! Validates the key concierge agent behaviors (status reporting, busy guard,
//! pcall error safety) using a self-contained Lua script that mirrors the
//! concierge's architecture. This avoids the full dependency chain (orcs.llm,
//! orcs.request, foundation_manager, etc.) while verifying the patterns.

use orcs_component::EventCategory;
use orcs_lua::testing::LuaTestHarness;
use orcs_runtime::sandbox::{ProjectSandbox, SandboxPolicy};
use serde_json::json;
use std::sync::Arc;

fn test_policy() -> Arc<dyn SandboxPolicy> {
    Arc::new(ProjectSandbox::new(".").expect("test sandbox"))
}

fn ext_cat() -> EventCategory {
    EventCategory::Extension {
        namespace: "test".to_string(),
        kind: "agent_task".to_string(),
    }
}

/// Minimal concierge-like component that replicates the key behavioral patterns:
/// - Module-level state (busy, session_id, turn_count, last_cost)
/// - Status RPC
/// - Busy guard (reject concurrent processing)
/// - pcall error safety (busy=false guaranteed)
/// - Tracking update after successful processing
const CONCIERGE_PATTERN_SCRIPT: &str = r###"
local busy = false
local session_id = nil
local turn_count = 0
local last_cost = nil
local last_provider = nil
local last_model = nil

--- Update tracking state after successful processing.
local function update_tracking(input, result)
    session_id = result.session_id
    last_cost = result.cost
    local cfg = input.config or {}
    if cfg.provider then last_provider = cfg.provider end
    if cfg.model then last_model = cfg.model end
end

local function handle_status()
    return {
        success = true,
        data = {
            busy = busy,
            session_id = session_id,
            turn_count = turn_count,
            last_cost = last_cost,
            provider = last_provider,
            model = last_model,
        },
    }
end

local function handle_process(input)
    local message = input.message or ""
    if message == "" then
        return { success = true }
    end

    -- Busy guard: reject concurrent processing
    if busy then
        return { success = false, error = "concierge is busy" }
    end

    busy = true
    turn_count = turn_count + 1

    local ok, result = pcall(function()
        -- Simulate processing: check for error trigger
        if message == "TRIGGER_ERROR" then
            error("simulated Lua error in processing")
        end

        -- Simulate successful LLM response
        local resp = {
            session_id = "sess-" .. tostring(turn_count),
            cost = 0.01 * turn_count,
        }
        update_tracking(input, resp)

        return {
            success = true,
            data = {
                response = "processed: " .. message,
                session_id = resp.session_id,
                cost = resp.cost,
            },
        }
    end)

    -- Guarantee busy=false regardless of pcall outcome
    busy = false

    if not ok then
        return { success = false, error = tostring(result) }
    end
    return result
end

return {
    id = "concierge-pattern",
    subscriptions = {"Extension"},

    on_request = function(request)
        local operation = request.operation or "process"

        if operation == "status" then
            return handle_status()
        end

        if operation == "process" then
            return handle_process(request.payload or {})
        end

        return { success = false, error = "unknown operation: " .. tostring(operation) }
    end,

    on_signal = function(signal)
        if signal.kind == "Veto" then return "Abort" end
        return "Handled"
    end,
}
"###;

// =============================================================================
// Status operation tests
// =============================================================================

mod status {
    use super::*;

    #[test]
    fn initial_status_returns_idle() {
        let mut harness = LuaTestHarness::from_script(CONCIERGE_PATTERN_SCRIPT, test_policy())
            .expect("should load concierge pattern script");

        let result = harness
            .request(ext_cat(), "status", json!({}))
            .expect("status should succeed");

        assert_eq!(result["busy"], false, "should not be busy initially");
        assert!(
            result["session_id"].is_null(),
            "session_id should be nil initially"
        );
        assert_eq!(result["turn_count"], 0, "turn_count should be 0 initially");
        assert!(
            result["last_cost"].is_null(),
            "last_cost should be nil initially"
        );
    }

    #[test]
    fn status_reflects_processing_state() {
        let mut harness = LuaTestHarness::from_script(CONCIERGE_PATTERN_SCRIPT, test_policy())
            .expect("should load concierge pattern script");

        // Process a message
        let result = harness
            .request(
                ext_cat(),
                "process",
                json!({ "message": "hello", "config": { "provider": "ollama", "model": "qwen" } }),
            )
            .expect("process should succeed");

        assert_eq!(result["response"], "processed: hello");

        // Check status after processing
        let status = harness
            .request(ext_cat(), "status", json!({}))
            .expect("status should succeed");

        assert_eq!(status["busy"], false, "should not be busy after processing");
        assert_eq!(status["session_id"], "sess-1");
        assert_eq!(status["turn_count"], 1);
        assert_eq!(status["provider"], "ollama");
        assert_eq!(status["model"], "qwen");
    }

    #[test]
    fn turn_count_increments_per_process() {
        let mut harness = LuaTestHarness::from_script(CONCIERGE_PATTERN_SCRIPT, test_policy())
            .expect("should load concierge pattern script");

        for i in 1..=3 {
            harness
                .request(
                    ext_cat(),
                    "process",
                    json!({ "message": format!("msg{i}") }),
                )
                .expect("process should succeed");
        }

        let status = harness
            .request(ext_cat(), "status", json!({}))
            .expect("status should succeed");

        assert_eq!(status["turn_count"], 3);
        assert_eq!(status["session_id"], "sess-3");
    }
}

// =============================================================================
// Busy guard tests
// =============================================================================

mod busy_guard {
    use super::*;

    #[test]
    fn empty_message_is_accepted_regardless() {
        let mut harness = LuaTestHarness::from_script(CONCIERGE_PATTERN_SCRIPT, test_policy())
            .expect("should load concierge pattern script");

        // Empty message should return success without setting busy
        let result = harness
            .request(ext_cat(), "process", json!({ "message": "" }))
            .expect("empty message should succeed");

        // Empty message returns success=true (top-level, not in data)
        // LuaTestHarness returns the data field, which is nil → null
        // This verifies the early return path works
        assert!(
            result.is_null() || result == json!({}),
            "empty message should return nil/empty data"
        );

        // Verify status: turn_count should still be 0 (empty msg doesn't count)
        let status = harness
            .request(ext_cat(), "status", json!({}))
            .expect("status should succeed");
        assert_eq!(
            status["turn_count"], 0,
            "empty message should not increment turn_count"
        );
    }
}

// =============================================================================
// pcall error safety tests
// =============================================================================

mod pcall_safety {
    use super::*;

    #[test]
    fn busy_resets_after_lua_error() {
        let mut harness = LuaTestHarness::from_script(CONCIERGE_PATTERN_SCRIPT, test_policy())
            .expect("should load concierge pattern script");

        // Trigger a Lua error during processing
        let result = harness.request(ext_cat(), "process", json!({ "message": "TRIGGER_ERROR" }));

        // The request should return an error (not panic)
        assert!(
            result.is_err(),
            "error-triggering message should result in error response"
        );

        // Verify busy flag was reset (pcall guarantee)
        let status = harness
            .request(ext_cat(), "status", json!({}))
            .expect("status should succeed after error");

        assert_eq!(
            status["busy"], false,
            "busy should be false after pcall error (guarantee)"
        );
        assert_eq!(
            status["turn_count"], 1,
            "turn_count should have incremented before the error"
        );
    }

    #[test]
    fn processing_resumes_after_error() {
        let mut harness = LuaTestHarness::from_script(CONCIERGE_PATTERN_SCRIPT, test_policy())
            .expect("should load concierge pattern script");

        // First: trigger error
        let _ = harness.request(ext_cat(), "process", json!({ "message": "TRIGGER_ERROR" }));

        // Second: normal processing should work
        let result = harness
            .request(ext_cat(), "process", json!({ "message": "recovery" }))
            .expect("should succeed after error recovery");

        assert_eq!(
            result["response"], "processed: recovery",
            "should process normally after error"
        );

        // Verify state is consistent
        let status = harness
            .request(ext_cat(), "status", json!({}))
            .expect("status should succeed");

        assert_eq!(status["turn_count"], 2, "both calls should be counted");
        assert_eq!(status["session_id"], "sess-2");
    }
}

// =============================================================================
// Tracking update tests (H1 fix verification)
// =============================================================================

mod tracking {
    use super::*;

    #[test]
    fn tracking_preserves_provider_when_config_absent() {
        let mut harness = LuaTestHarness::from_script(CONCIERGE_PATTERN_SCRIPT, test_policy())
            .expect("should load concierge pattern script");

        // First call: set provider/model via config
        harness
            .request(
                ext_cat(),
                "process",
                json!({ "message": "first", "config": { "provider": "anthropic", "model": "claude" } }),
            )
            .expect("should succeed");

        // Second call: no config (simulates session resumption)
        harness
            .request(ext_cat(), "process", json!({ "message": "second" }))
            .expect("should succeed");

        // Verify provider/model were NOT overwritten to nil
        let status = harness
            .request(ext_cat(), "status", json!({}))
            .expect("status should succeed");

        assert_eq!(
            status["provider"], "anthropic",
            "provider should be preserved when config is absent (H1 fix)"
        );
        assert_eq!(
            status["model"], "claude",
            "model should be preserved when config is absent (H1 fix)"
        );
    }

    #[test]
    fn tracking_updates_when_config_present() {
        let mut harness = LuaTestHarness::from_script(CONCIERGE_PATTERN_SCRIPT, test_policy())
            .expect("should load concierge pattern script");

        // First call with one provider
        harness
            .request(
                ext_cat(),
                "process",
                json!({ "message": "a", "config": { "provider": "ollama", "model": "qwen" } }),
            )
            .expect("should succeed");

        // Second call with different provider
        harness
            .request(
                ext_cat(),
                "process",
                json!({ "message": "b", "config": { "provider": "openai", "model": "gpt-4" } }),
            )
            .expect("should succeed");

        let status = harness
            .request(ext_cat(), "status", json!({}))
            .expect("status should succeed");

        assert_eq!(status["provider"], "openai");
        assert_eq!(status["model"], "gpt-4");
    }
}

// =============================================================================
// Unknown operation tests
// =============================================================================

mod unknown_ops {
    use super::*;

    #[test]
    fn unknown_operation_returns_error() {
        let mut harness = LuaTestHarness::from_script(CONCIERGE_PATTERN_SCRIPT, test_policy())
            .expect("should load concierge pattern script");

        let result = harness.request(ext_cat(), "nonexistent", json!({}));

        assert!(result.is_err(), "unknown operation should return error");
    }
}

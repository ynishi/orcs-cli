//! Integration tests using LuaTestHarness.
//!
//! Demonstrates testing Lua-based components with the test harness.

use orcs_component::{EventCategory, Status};
use orcs_event::SignalResponse;
use orcs_lua::testing::LuaTestHarness;
use serde_json::json;

// =============================================================================
// Basic Lua Component Tests
// =============================================================================

mod basic {
    use super::*;

    const ECHO_SCRIPT: &str = r#"
        return {
            id = "echo",
            subscriptions = {"Echo"},
            on_request = function(req)
                if req.operation == "echo" then
                    return { success = true, data = req.payload }
                elseif req.operation == "uppercase" then
                    local text = req.payload.text or ""
                    return { success = true, data = { text = string.upper(text) } }
                end
                return { success = false, error = "Unknown operation: " .. req.operation }
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
    fn echo_request() {
        let mut harness = LuaTestHarness::from_script(ECHO_SCRIPT).unwrap();

        let result = harness.request(EventCategory::Echo, "echo", json!({"msg": "hello"}));

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), json!({"msg": "hello"}));
    }

    #[test]
    fn uppercase_request() {
        let mut harness = LuaTestHarness::from_script(ECHO_SCRIPT).unwrap();

        let result = harness.request(
            EventCategory::Echo,
            "uppercase",
            json!({"text": "hello world"}),
        );

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), json!({"text": "HELLO WORLD"}));
    }

    #[test]
    fn unknown_operation_error() {
        let mut harness = LuaTestHarness::from_script(ECHO_SCRIPT).unwrap();

        let result = harness.request(EventCategory::Echo, "unknown", json!({}));

        assert!(result.is_err());
    }

    #[test]
    fn veto_aborts() {
        let mut harness = LuaTestHarness::from_script(ECHO_SCRIPT).unwrap();

        let response = harness.veto();

        assert_eq!(response, SignalResponse::Abort);
        assert_eq!(harness.status(), Status::Aborted);
    }

    #[test]
    fn cancel_handled() {
        let mut harness = LuaTestHarness::from_script(ECHO_SCRIPT).unwrap();

        let response = harness.cancel();

        assert_eq!(response, SignalResponse::Handled);
        assert_eq!(harness.status(), Status::Idle);
    }

    #[test]
    fn subscriptions() {
        let harness = LuaTestHarness::from_script(ECHO_SCRIPT).unwrap();

        assert_eq!(harness.subscriptions(), &[EventCategory::Echo]);
    }

    #[test]
    fn component_id() {
        let harness = LuaTestHarness::from_script(ECHO_SCRIPT).unwrap();

        assert_eq!(harness.id().name, "echo");
        assert_eq!(harness.id().namespace, "lua");
    }
}

// =============================================================================
// Stateful Lua Component Tests
// =============================================================================

mod stateful {
    use super::*;

    const COUNTER_SCRIPT: &str = r#"
        local count = 0

        return {
            id = "counter",
            subscriptions = {"Echo"},
            on_request = function(req)
                if req.operation == "increment" then
                    count = count + 1
                    return { success = true, data = { count = count } }
                elseif req.operation == "get" then
                    return { success = true, data = { count = count } }
                elseif req.operation == "reset" then
                    count = 0
                    return { success = true, data = { count = count } }
                end
                return { success = false, error = "Unknown operation" }
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
    fn increment_counter() {
        let mut harness = LuaTestHarness::from_script(COUNTER_SCRIPT).unwrap();

        harness
            .request(EventCategory::Echo, "increment", json!({}))
            .unwrap();
        harness
            .request(EventCategory::Echo, "increment", json!({}))
            .unwrap();
        harness
            .request(EventCategory::Echo, "increment", json!({}))
            .unwrap();

        let result = harness
            .request(EventCategory::Echo, "get", json!({}))
            .unwrap();
        assert_eq!(result["count"], 3);
    }

    #[test]
    fn reset_counter() {
        let mut harness = LuaTestHarness::from_script(COUNTER_SCRIPT).unwrap();

        harness
            .request(EventCategory::Echo, "increment", json!({}))
            .unwrap();
        harness
            .request(EventCategory::Echo, "increment", json!({}))
            .unwrap();

        let result = harness
            .request(EventCategory::Echo, "reset", json!({}))
            .unwrap();
        assert_eq!(result["count"], 0);
    }
}

// =============================================================================
// Lifecycle Tests
// =============================================================================

mod lifecycle {
    use super::*;

    const LIFECYCLE_SCRIPT: &str = r#"
        local state = {
            initialized = false,
            shutdown = false,
            request_count = 0
        }

        return {
            id = "lifecycle-test",
            subscriptions = {"Lifecycle"},

            init = function()
                state.initialized = true
            end,

            shutdown = function()
                state.shutdown = true
            end,

            on_request = function(req)
                state.request_count = state.request_count + 1

                if req.operation == "get_state" then
                    return {
                        success = true,
                        data = {
                            initialized = state.initialized,
                            shutdown = state.shutdown,
                            request_count = state.request_count
                        }
                    }
                end
                return { success = true }
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
    fn init_called() {
        let mut harness = LuaTestHarness::from_script(LIFECYCLE_SCRIPT).unwrap();

        assert!(harness.init().is_ok());

        let result = harness
            .request(EventCategory::Lifecycle, "get_state", json!({}))
            .unwrap();
        assert_eq!(result["initialized"], true);
    }

    #[test]
    fn shutdown_called() {
        let mut harness = LuaTestHarness::from_script(LIFECYCLE_SCRIPT).unwrap();

        harness.init().ok();
        harness.shutdown();

        // After shutdown, component may not be in a valid state
        // but the shutdown callback should have been called
        assert_eq!(harness.status(), Status::Idle);
    }

    #[test]
    fn request_count_tracked() {
        let mut harness = LuaTestHarness::from_script(LIFECYCLE_SCRIPT).unwrap();

        harness
            .request(EventCategory::Lifecycle, "any", json!({}))
            .ok();
        harness
            .request(EventCategory::Lifecycle, "any", json!({}))
            .ok();
        harness
            .request(EventCategory::Lifecycle, "any", json!({}))
            .ok();

        let result = harness
            .request(EventCategory::Lifecycle, "get_state", json!({}))
            .unwrap();
        assert_eq!(result["request_count"], 4); // 3 + 1 for get_state
    }
}

// =============================================================================
// Signal Response Tests
// =============================================================================

mod signals {
    use super::*;

    const SIGNAL_SCRIPT: &str = r#"
        return {
            id = "signal-test",
            subscriptions = {"Hil"},
            on_request = function(req)
                return { success = true }
            end,
            on_signal = function(sig)
                if sig.kind == "Veto" then
                    return "Abort"
                elseif sig.kind == "Cancel" then
                    return "Handled"
                elseif sig.kind == "Approve" then
                    return "Handled"
                elseif sig.kind == "Reject" then
                    return "Handled"
                end
                return "Ignored"
            end,
        }
    "#;

    #[test]
    fn veto_aborts() {
        let mut harness = LuaTestHarness::from_script(SIGNAL_SCRIPT).unwrap();

        let response = harness.veto();

        assert_eq!(response, SignalResponse::Abort);
        assert_eq!(harness.status(), Status::Aborted);
    }

    #[test]
    fn cancel_handled() {
        let mut harness = LuaTestHarness::from_script(SIGNAL_SCRIPT).unwrap();

        let response = harness.cancel();

        assert_eq!(response, SignalResponse::Handled);
    }

    #[test]
    fn approve_handled() {
        let mut harness = LuaTestHarness::from_script(SIGNAL_SCRIPT).unwrap();

        let response = harness.approve("test-approval");

        assert_eq!(response, SignalResponse::Handled);
    }

    #[test]
    fn reject_handled() {
        let mut harness = LuaTestHarness::from_script(SIGNAL_SCRIPT).unwrap();

        let response = harness.reject("test-approval", Some("reason".to_string()));

        assert_eq!(response, SignalResponse::Handled);
    }
}

// =============================================================================
// Error Handling Tests
// =============================================================================

mod errors {
    use super::*;

    #[test]
    fn invalid_script_syntax() {
        let result = LuaTestHarness::from_script("invalid lua syntax {{{");
        assert!(result.is_err());
    }

    #[test]
    fn missing_id() {
        let script = r#"
            return {
                subscriptions = {"Echo"},
                on_request = function(req) return { success = true } end,
                on_signal = function(sig) return "Ignored" end,
            }
        "#;

        let result = LuaTestHarness::from_script(script);
        assert!(result.is_err());
    }

    #[test]
    fn missing_subscriptions() {
        let script = r#"
            return {
                id = "incomplete",
                on_request = function(req) return { success = true } end,
                on_signal = function(sig) return "Ignored" end,
            }
        "#;

        let result = LuaTestHarness::from_script(script);
        assert!(result.is_err());
    }

    #[test]
    fn missing_on_request() {
        let script = r#"
            return {
                id = "incomplete",
                subscriptions = {"Echo"},
                on_signal = function(sig) return "Ignored" end,
            }
        "#;

        let result = LuaTestHarness::from_script(script);
        assert!(result.is_err());
    }

    #[test]
    fn missing_on_signal() {
        let script = r#"
            return {
                id = "incomplete",
                subscriptions = {"Echo"},
                on_request = function(req) return { success = true } end,
            }
        "#;

        let result = LuaTestHarness::from_script(script);
        assert!(result.is_err());
    }
}

// =============================================================================
// Request/Signal Logging Tests
// =============================================================================

mod logging {
    use super::*;

    const SIMPLE_SCRIPT: &str = r#"
        return {
            id = "logging-test",
            subscriptions = {"Echo"},
            on_request = function(req)
                return { success = true, data = req.payload }
            end,
            on_signal = function(sig)
                if sig.kind == "Veto" then return "Abort" end
                return "Handled"
            end,
        }
    "#;

    #[test]
    fn request_log_captured() {
        let mut harness = LuaTestHarness::from_script(SIMPLE_SCRIPT).unwrap();

        harness.request(EventCategory::Echo, "op1", json!(1)).ok();
        harness.request(EventCategory::Echo, "op2", json!(2)).ok();
        harness.request(EventCategory::Echo, "op3", json!(3)).ok();

        assert_eq!(harness.request_log().len(), 3);
        assert_eq!(harness.request_log()[0].operation, "op1");
        assert_eq!(harness.request_log()[1].operation, "op2");
        assert_eq!(harness.request_log()[2].operation, "op3");
    }

    #[test]
    fn signal_log_captured() {
        let mut harness = LuaTestHarness::from_script(SIMPLE_SCRIPT).unwrap();

        harness.cancel();
        harness.approve("test");
        harness.reject("test", None);

        assert_eq!(harness.signal_log().len(), 3);
    }

    #[test]
    fn clear_logs() {
        let mut harness = LuaTestHarness::from_script(SIMPLE_SCRIPT).unwrap();

        harness.request(EventCategory::Echo, "op", json!({})).ok();
        harness.cancel();

        assert_eq!(harness.request_log().len(), 1);
        assert_eq!(harness.signal_log().len(), 1);

        harness.clear_logs();

        assert_eq!(harness.request_log().len(), 0);
        assert_eq!(harness.signal_log().len(), 0);
    }

    #[test]
    fn script_source_accessible() {
        let harness = LuaTestHarness::from_script(SIMPLE_SCRIPT).unwrap();

        assert!(harness.script_source().is_some());
        assert!(harness.script_source().unwrap().contains("logging-test"));
    }
}

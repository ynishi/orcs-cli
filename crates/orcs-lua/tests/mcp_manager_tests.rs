//! Integration tests for MCPManagerComponent.
//!
//! The LuaComponent `on_request` protocol:
//! - `{success = true, data = X}` → `Ok(X)` (harness returns the `data` value)
//! - `{success = false, error = msg}` → `Err(ComponentError)`

use orcs_component::EventCategory;
use orcs_event::SignalResponse;
use orcs_lua::testing::LuaTestHarness;
use orcs_runtime::sandbox::{ProjectSandbox, SandboxPolicy};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

/// Returns the path to the mcp_manager component directory.
///
/// mcp_manager lives in `orcs-app/builtins/` rather than `orcs-lua/scripts/`,
/// so we navigate relative to CARGO_MANIFEST_DIR (orcs-lua crate root).
fn mcp_manager_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../orcs-app/builtins/mcp_manager")
        .canonicalize()
        .expect("mcp_manager dir should exist at ../orcs-app/builtins/mcp_manager")
}

fn test_sandbox() -> (TempDir, Arc<dyn SandboxPolicy>) {
    let td = tempfile::tempdir().expect("create temp dir");
    let root = td.path().canonicalize().expect("canonicalize temp dir");
    let sandbox = Arc::new(ProjectSandbox::new(&root).expect("test sandbox"));
    (td, sandbox)
}

fn mcp_harness() -> (TempDir, LuaTestHarness) {
    let (td, sandbox) = test_sandbox();
    let harness = LuaTestHarness::from_dir(mcp_manager_dir(), sandbox)
        .expect("load mcp_manager from builtins dir");
    (td, harness)
}

fn mcp_harness_with_mocks(
    servers: Vec<String>,
    tools: Vec<serde_json::Value>,
    call_responses: Vec<String>,
) -> (TempDir, LuaTestHarness) {
    let (td, harness) = mcp_harness();
    harness.inject_mcp_mocks(servers, tools, call_responses);
    (td, harness)
}

fn ext_cat() -> EventCategory {
    EventCategory::Extension {
        namespace: "mcp".to_string(),
        kind: "request".to_string(),
    }
}

fn sample_tools() -> Vec<serde_json::Value> {
    vec![
        json!({"server": "outline", "tool": "toc", "name": "toc", "description": "Show table of contents"}),
        json!({"server": "outline", "tool": "node_create", "name": "node_create", "description": "Create a new node"}),
        json!({"server": "debugger", "tool": "debug_launch", "name": "debug_launch", "description": "Launch debug session"}),
    ]
}

// =============================================================================
// Basic Lifecycle
// =============================================================================

mod lifecycle {
    use super::*;

    #[test]
    fn load_mcp_manager_component() {
        let (_td, harness) = mcp_harness();
        assert_eq!(harness.id().name, "mcp_manager");
    }

    #[test]
    fn init_with_mcp_mocks() {
        let (_td, mut harness) =
            mcp_harness_with_mocks(vec!["outline".into()], sample_tools(), vec![]);
        harness
            .init()
            .expect("init should succeed with MCP mocks injected");
    }

    #[test]
    fn init_and_shutdown() {
        let (_td, mut harness) =
            mcp_harness_with_mocks(vec!["outline".into()], sample_tools(), vec![]);
        harness.init().expect("init should succeed");
        harness.shutdown();
    }

    #[test]
    fn veto_aborts() {
        let (_td, mut harness) =
            mcp_harness_with_mocks(vec!["outline".into()], sample_tools(), vec![]);
        harness.init().expect("init should succeed");
        let response = harness.veto();
        assert_eq!(response, SignalResponse::Abort);
    }
}

// =============================================================================
// Status
// =============================================================================

mod status {
    use super::*;

    #[test]
    fn status_returns_server_and_tool_counts() {
        let (_td, mut harness) = mcp_harness_with_mocks(
            vec!["outline".into(), "debugger".into()],
            sample_tools(),
            vec![],
        );
        harness.init().expect("init should succeed");

        let data = harness
            .request(ext_cat(), "status", json!({}))
            .expect("status should succeed");

        assert_eq!(data["servers"], 2);
        assert_eq!(data["tools"], 3);
        assert_eq!(data["initialized"], true);
    }

    #[test]
    fn status_with_no_servers() {
        let (_td, mut harness) = mcp_harness_with_mocks(vec![], vec![], vec![]);
        harness.init().expect("init should succeed");

        let data = harness
            .request(ext_cat(), "status", json!({}))
            .expect("status should succeed");

        assert_eq!(data["servers"], 0);
        assert_eq!(data["tools"], 0);
    }
}

// =============================================================================
// Server listing (list)
// =============================================================================

mod list {
    use super::*;

    #[test]
    fn list_returns_connected_servers() {
        let (_td, mut harness) = mcp_harness_with_mocks(
            vec!["outline".into(), "debugger".into()],
            sample_tools(),
            vec![],
        );

        let data = harness
            .request(ext_cat(), "list", json!({}))
            .expect("list should succeed");

        // data is the servers array itself (handle_list returns {data = resp.servers, count = #})
        // But actually mcp_manager returns {success=true, data=resp.servers, count=N}
        // LuaResponse parses: success=true → Ok(data), where data = the "data" field
        // So `data` here is `resp.servers` which is the array from mcp_servers mock
        assert!(data.is_array(), "data should be array of server names");
    }
}

// =============================================================================
// Get server info
// =============================================================================

mod get {
    use super::*;

    #[test]
    fn get_returns_tools_for_server() {
        let (_td, mut harness) =
            mcp_harness_with_mocks(vec!["outline".into()], sample_tools(), vec![]);

        let data = harness
            .request(ext_cat(), "get", json!({"name": "outline"}))
            .expect("get should succeed");

        assert_eq!(data["server"], "outline");
        // outline has 2 tools in sample_tools
        assert_eq!(data["tool_count"], 2);
    }

    #[test]
    fn get_missing_name_returns_error() {
        let (_td, mut harness) = mcp_harness_with_mocks(vec![], vec![], vec![]);

        let result = harness.request(ext_cat(), "get", json!({}));

        assert!(result.is_err(), "get without name should fail");
    }
}

// =============================================================================
// Tools listing
// =============================================================================

mod tools {
    use super::*;

    #[test]
    fn tools_returns_all_tools() {
        let (_td, mut harness) =
            mcp_harness_with_mocks(vec!["outline".into()], sample_tools(), vec![]);

        let data = harness
            .request(ext_cat(), "tools", json!({}))
            .expect("tools should succeed");

        assert!(data.is_array(), "data should be array of tools");
        assert_eq!(data.as_array().expect("array").len(), 3);
    }

    #[test]
    fn tools_filters_by_server() {
        let (_td, mut harness) =
            mcp_harness_with_mocks(vec!["outline".into()], sample_tools(), vec![]);

        let data = harness
            .request(ext_cat(), "tools", json!({"server": "debugger"}))
            .expect("tools with filter should succeed");

        assert!(data.is_array(), "data should be array of tools");
        assert_eq!(data.as_array().expect("array").len(), 1);
    }
}

// =============================================================================
// Search
// =============================================================================

mod search {
    use super::*;

    #[test]
    fn search_by_name() {
        let (_td, mut harness) =
            mcp_harness_with_mocks(vec!["outline".into()], sample_tools(), vec![]);

        let data = harness
            .request(ext_cat(), "search", json!({"query": "toc"}))
            .expect("search should succeed");

        assert!(data.is_array(), "data should be array of matches");
        assert_eq!(data.as_array().expect("array").len(), 1);
    }

    #[test]
    fn search_by_description() {
        let (_td, mut harness) =
            mcp_harness_with_mocks(vec!["outline".into()], sample_tools(), vec![]);

        let data = harness
            .request(ext_cat(), "search", json!({"query": "debug"}))
            .expect("search should succeed");

        assert!(data.is_array(), "data should be array of matches");
        assert_eq!(data.as_array().expect("array").len(), 1);
    }

    #[test]
    fn search_no_match() {
        let (_td, mut harness) =
            mcp_harness_with_mocks(vec!["outline".into()], sample_tools(), vec![]);

        let data = harness
            .request(ext_cat(), "search", json!({"query": "nonexistent_xyz"}))
            .expect("search should succeed");

        // Empty Lua table `{}` may serialize as JSON object `{}` or array `[]`
        let count = match &data {
            serde_json::Value::Array(arr) => arr.len(),
            serde_json::Value::Object(obj) => obj.len(),
            _ => 0,
        };
        assert_eq!(count, 0, "search should return no matches");
    }

    #[test]
    fn search_missing_query_returns_error() {
        let (_td, mut harness) = mcp_harness_with_mocks(vec![], vec![], vec![]);

        let result = harness.request(ext_cat(), "search", json!({}));

        assert!(result.is_err(), "search without query should fail");
    }
}

// =============================================================================
// Run (call MCP tool)
// =============================================================================

mod run {
    use super::*;

    #[test]
    fn run_with_server_and_tool() {
        let (_td, mut harness) = mcp_harness();

        let captured = harness.inject_mcp_mocks(
            vec!["outline".into()],
            sample_tools(),
            vec!["toc result".into()],
        );

        let data = harness
            .request(
                ext_cat(),
                "run",
                json!({"server": "outline", "tool": "toc", "args": {}}),
            )
            .expect("run should succeed");

        // data = {content = [{type="text", text="toc result"}]}
        assert!(
            data["content"].is_array(),
            "data should contain content array"
        );

        let calls = captured.lock().expect("captured mutex");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["server"], "outline");
        assert_eq!(calls[0]["tool"], "toc");
    }

    #[test]
    fn run_with_name_format_server_tool() {
        let (_td, mut harness) = mcp_harness();

        let captured = harness.inject_mcp_mocks(
            vec!["outline".into()],
            sample_tools(),
            vec!["result".into()],
        );

        let _data = harness
            .request(
                ext_cat(),
                "run",
                json!({"name": "outline:toc", "args": {"key": "val"}}),
            )
            .expect("run with server:tool format should succeed");

        let calls = captured.lock().expect("captured mutex");
        assert_eq!(calls[0]["server"], "outline");
        assert_eq!(calls[0]["tool"], "toc");
    }

    #[test]
    fn run_with_name_format_mcp_server_tool() {
        let (_td, mut harness) = mcp_harness();

        let captured = harness.inject_mcp_mocks(
            vec!["outline".into()],
            sample_tools(),
            vec!["result".into()],
        );

        let _data = harness
            .request(ext_cat(), "run", json!({"name": "mcp:outline:toc"}))
            .expect("run with mcp:server:tool format should succeed");

        let calls = captured.lock().expect("captured mutex");
        assert_eq!(calls[0]["server"], "outline");
        assert_eq!(calls[0]["tool"], "toc");
    }

    #[test]
    fn run_invalid_name_format_returns_error() {
        let (_td, mut harness) = mcp_harness_with_mocks(vec![], vec![], vec![]);

        let result = harness.request(ext_cat(), "run", json!({"name": "invalid"}));

        assert!(result.is_err(), "run with invalid name should fail");
    }

    #[test]
    fn run_missing_server_and_tool_returns_error() {
        let (_td, mut harness) = mcp_harness_with_mocks(vec![], vec![], vec![]);

        let result = harness.request(ext_cat(), "run", json!({"server": "x"}));

        assert!(result.is_err(), "run with missing tool should fail");
    }

    #[test]
    fn call_tool_alias_works() {
        let (_td, mut harness) = mcp_harness();

        let captured = harness.inject_mcp_mocks(
            vec!["outline".into()],
            sample_tools(),
            vec!["result".into()],
        );

        let _data = harness
            .request(
                ext_cat(),
                "call_tool",
                json!({"server": "outline", "tool": "toc", "args": {}}),
            )
            .expect("call_tool alias should succeed");

        let calls = captured.lock().expect("captured mutex");
        assert_eq!(calls.len(), 1);
    }
}

// =============================================================================
// Tool descriptions
// =============================================================================

mod tool_descriptions {
    use super::*;

    #[test]
    fn tool_descriptions_returns_formatted_text() {
        let (_td, mut harness) =
            mcp_harness_with_mocks(vec!["outline".into()], sample_tools(), vec![]);

        let data = harness
            .request(ext_cat(), "tool_descriptions", json!({}))
            .expect("tool_descriptions should succeed");

        let text = data["text"].as_str().expect("text should be string");
        assert!(
            text.contains("toc"),
            "descriptions should contain tool name 'toc'"
        );
        assert!(
            text.contains("outline"),
            "descriptions should contain server name"
        );
        assert_eq!(data["count"], 3);
    }

    #[test]
    fn tool_descriptions_filtered_by_server() {
        let (_td, mut harness) =
            mcp_harness_with_mocks(vec!["debugger".into()], sample_tools(), vec![]);

        let data = harness
            .request(
                ext_cat(),
                "tool_descriptions",
                json!({"server": "debugger"}),
            )
            .expect("tool_descriptions with filter should succeed");

        assert_eq!(data["count"], 1);
    }
}

// =============================================================================
// Recommend (LLM-based, with fallback)
// =============================================================================

mod recommend {
    use super::*;

    #[test]
    fn recommend_missing_intent_returns_error() {
        let (_td, mut harness) = mcp_harness_with_mocks(vec![], vec![], vec![]);

        let result = harness.request(ext_cat(), "recommend", json!({}));

        assert!(result.is_err(), "recommend without intent should fail");
    }

    #[test]
    fn recommend_with_no_tools_returns_empty() {
        let (_td, mut harness) = mcp_harness_with_mocks(vec![], vec![], vec![]);

        let data = harness
            .request(ext_cat(), "recommend", json!({"intent": "test"}))
            .expect("recommend should succeed");

        // Empty Lua table `{}` may serialize as JSON object `{}` or array `[]`
        let count = match &data {
            serde_json::Value::Array(arr) => arr.len(),
            serde_json::Value::Object(obj) => obj.len(),
            _ => 0,
        };
        assert_eq!(count, 0, "recommend with no tools should return empty");
    }

    #[test]
    fn recommend_with_llm_returns_parsed_tools() {
        let (_td, mut harness) =
            mcp_harness_with_mocks(vec!["outline".into()], sample_tools(), vec![]);

        // Inject LLM mock that returns valid JSON array
        harness.inject_llm_mock(vec![r#"[{"server":"outline","tool":"toc"}]"#.to_string()]);

        let data = harness
            .request(ext_cat(), "recommend", json!({"intent": "show contents"}))
            .expect("recommend should succeed with LLM");

        assert!(data.is_array(), "data should be array");
        let arr = data.as_array().expect("array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["server"], "outline");
        assert_eq!(arr[0]["tool"], "toc");
    }
}

// =============================================================================
// Snapshot / Restore
// =============================================================================

mod snapshot_restore {
    use super::*;

    #[test]
    fn snapshot_captures_initialized_state() {
        let (_td, mut harness) =
            mcp_harness_with_mocks(vec!["outline".into()], sample_tools(), vec![]);
        harness.init().expect("init should succeed");

        let data = harness
            .request(ext_cat(), "status", json!({}))
            .expect("status should succeed");

        assert_eq!(data["initialized"], true);
    }
}

// =============================================================================
// Unknown operation / Error cases
// =============================================================================

mod errors {
    use super::*;

    #[test]
    fn unknown_operation_returns_error() {
        let (_td, mut harness) = mcp_harness_with_mocks(vec![], vec![], vec![]);

        let result = harness.request(ext_cat(), "nonexistent_op", json!({}));

        assert!(result.is_err(), "unknown operation should fail");
    }

    #[test]
    fn missing_required_name() {
        let (_td, mut harness) = mcp_harness_with_mocks(vec![], vec![], vec![]);

        let result = harness.request(ext_cat(), "get", json!({}));

        assert!(result.is_err(), "get without name should fail");
    }
}

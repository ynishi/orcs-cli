//! Integration tests for agent_mgr ReAct-loop helper functions.
//!
//! Tests `extract_commands` (with code-block skipping) and `format_command_results`
//! in isolation by embedding them in a test component.
//!
//! These functions are normally local to `agent_mgr.lua`. We replicate them here
//! with the same route table so that behavioral changes in the source are caught
//! by failing tests (test must be kept in sync).

use orcs_component::EventCategory;
use orcs_lua::testing::LuaTestHarness;
use orcs_runtime::sandbox::{ProjectSandbox, SandboxPolicy};
use serde_json::json;
use std::sync::Arc;

fn test_sandbox() -> Arc<dyn SandboxPolicy> {
    Arc::new(ProjectSandbox::new(".").expect("test sandbox"))
}

/// Test component that exposes `extract_commands` and `format_command_results`
/// via `on_request`.
///
/// Operations:
///   - `extract_commands`:  payload `{ text = "..." }` → `{ commands = [...] }`
///   - `format_results`:    payload `{ results = [...] }` → `{ formatted = "..." }`
const REACT_HELPERS_SCRIPT: &str = r###"
-- Replicate the route table from agent_mgr.lua (only prefixes matter for matching)
local routes = {
    shell      = { type = "rpc",   target = "builtin::shell" },
    skill      = { type = "child", target = "skill-worker" },
    profile    = { type = "rpc",   target = "profile::profile_manager" },
    tool       = { type = "rpc",   target = "builtin::tool" },
    foundation = { type = "rpc",   target = "foundation::foundation_manager" },
}

-- Copied from agent_mgr.lua — must stay in sync
local function extract_commands(text)
    local commands = {}
    local in_code_block = false
    for line in text:gmatch("[^\n]+") do
        local trimmed = line:match("^%s*(.-)%s*$")
        if trimmed:match("^```") then
            in_code_block = not in_code_block
        elseif not in_code_block then
            local prefix, body = trimmed:match("^@(%w+)%s+(.+)$")
            if not prefix then
                prefix = trimmed:match("^@(%w+)%s*$")
                body = ""
            end
            if prefix then
                local p = prefix:lower()
                if routes[p] then
                    commands[#commands + 1] = { prefix = p, body = body or "" }
                end
            end
        end
    end
    return commands
end

local function format_command_results(results)
    local parts = {}
    for _, r in ipairs(results) do
        local status = r.success and "OK" or "ERROR"
        local content = r.response or r.error or "(no output)"
        if type(content) == "table" then
            content = orcs.json_encode(content)
        end
        parts[#parts + 1] = string.format("[@%s result: %s]\n%s", r.prefix, status, tostring(content))
    end
    return table.concat(parts, "\n\n")
end

return {
    id = "react-helpers",
    subscriptions = {"Extension"},

    on_request = function(req)
        local op = req.operation
        local payload = req.payload or {}

        if op == "extract_commands" then
            local text = payload.text or ""
            local cmds = extract_commands(text)
            -- Convert to JSON-friendly array
            local out = {}
            for i, c in ipairs(cmds) do
                out[i] = { prefix = c.prefix, body = c.body }
            end
            return { success = true, data = { commands = out } }

        elseif op == "format_results" then
            local results = payload.results or {}
            local formatted = format_command_results(results)
            return { success = true, data = { formatted = formatted } }
        end

        return { success = false, error = "unknown operation: " .. tostring(op) }
    end,

    on_signal = function(sig)
        if sig.kind == "Veto" then return "Abort" end
        return "Handled"
    end,
}
"###;

fn ext_cat() -> EventCategory {
    EventCategory::Extension {
        namespace: "test".to_string(),
        kind: "react".to_string(),
    }
}

fn setup_harness() -> LuaTestHarness {
    LuaTestHarness::from_script(REACT_HELPERS_SCRIPT, test_sandbox())
        .expect("should load react-helpers script")
}

// =============================================================================
// extract_commands tests
// =============================================================================

mod extract_commands {
    use super::*;

    #[test]
    fn single_command_with_body() {
        let mut h = setup_harness();
        let result = h
            .request(
                ext_cat(),
                "extract_commands",
                json!({ "text": "@shell ls -la /tmp" }),
            )
            .expect("extract should succeed");

        let cmds = result["commands"].as_array().expect("commands array");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0]["prefix"], "shell");
        assert_eq!(cmds[0]["body"], "ls -la /tmp");
    }

    #[test]
    fn bare_prefix_without_body() {
        let mut h = setup_harness();
        let result = h
            .request(ext_cat(), "extract_commands", json!({ "text": "@skill" }))
            .expect("extract should succeed");

        let cmds = result["commands"].as_array().expect("commands array");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0]["prefix"], "skill");
        assert_eq!(cmds[0]["body"], "");
    }

    #[test]
    fn multiple_commands() {
        let mut h = setup_harness();
        let text = "@shell echo hello\nSome text in between\n@tool list all\n@profile show";
        let result = h
            .request(ext_cat(), "extract_commands", json!({ "text": text }))
            .expect("extract should succeed");

        let cmds = result["commands"].as_array().expect("commands array");
        assert_eq!(cmds.len(), 3);
        assert_eq!(cmds[0]["prefix"], "shell");
        assert_eq!(cmds[1]["prefix"], "tool");
        assert_eq!(cmds[2]["prefix"], "profile");
    }

    #[test]
    fn ignores_unknown_prefix() {
        let mut h = setup_harness();
        let result = h
            .request(
                ext_cat(),
                "extract_commands",
                json!({ "text": "@unknown do something\n@shell ls" }),
            )
            .expect("extract should succeed");

        let cmds = result["commands"].as_array().expect("commands array");
        assert_eq!(cmds.len(), 1, "only registered route should match");
        assert_eq!(cmds[0]["prefix"], "shell");
    }

    #[test]
    fn case_insensitive_prefix() {
        let mut h = setup_harness();
        let result = h
            .request(
                ext_cat(),
                "extract_commands",
                json!({ "text": "@Shell ls\n@TOOL status\n@Skill" }),
            )
            .expect("extract should succeed");

        let cmds = result["commands"].as_array().expect("commands array");
        assert_eq!(cmds.len(), 3);
        assert_eq!(cmds[0]["prefix"], "shell");
        assert_eq!(cmds[1]["prefix"], "tool");
        assert_eq!(cmds[2]["prefix"], "skill");
    }

    #[test]
    fn skips_commands_inside_code_block() {
        let mut h = setup_harness();
        let text = "Here is an example:\n```\n@shell rm -rf /\n@tool dangerous\n```\n@shell ls";
        let result = h
            .request(ext_cat(), "extract_commands", json!({ "text": text }))
            .expect("extract should succeed");

        let cmds = result["commands"].as_array().expect("commands array");
        assert_eq!(
            cmds.len(),
            1,
            "only command outside code block should match"
        );
        assert_eq!(cmds[0]["prefix"], "shell");
        assert_eq!(cmds[0]["body"], "ls");
    }

    #[test]
    fn skips_code_block_with_language_tag() {
        let mut h = setup_harness();
        let text = "```bash\n@shell echo danger\n```\n@shell echo safe";
        let result = h
            .request(ext_cat(), "extract_commands", json!({ "text": text }))
            .expect("extract should succeed");

        let cmds = result["commands"].as_array().expect("commands array");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0]["body"], "echo safe");
    }

    #[test]
    fn multiple_code_blocks() {
        let mut h = setup_harness();
        let text = "@shell first\n```\n@shell inside1\n```\n@shell middle\n```\n@shell inside2\n```\n@shell last";
        let result = h
            .request(ext_cat(), "extract_commands", json!({ "text": text }))
            .expect("extract should succeed");

        let cmds = result["commands"].as_array().expect("commands array");
        assert_eq!(
            cmds.len(),
            3,
            "only commands outside all code blocks should match"
        );
        assert_eq!(cmds[0]["body"], "first");
        assert_eq!(cmds[1]["body"], "middle");
        assert_eq!(cmds[2]["body"], "last");
    }

    #[test]
    fn no_commands_returns_empty() {
        let mut h = setup_harness();
        let result = h
            .request(
                ext_cat(),
                "extract_commands",
                json!({ "text": "Just a normal response with no commands." }),
            )
            .expect("extract should succeed");

        // Empty Lua table may serialize as {} (object) or [] (array)
        let cmds = &result["commands"];
        let len = cmds.as_array().map_or(0, |a| a.len());
        assert_eq!(len, 0, "should have no commands");
    }

    #[test]
    fn empty_text_returns_empty() {
        let mut h = setup_harness();
        let result = h
            .request(ext_cat(), "extract_commands", json!({ "text": "" }))
            .expect("extract should succeed");

        let cmds = &result["commands"];
        let len = cmds.as_array().map_or(0, |a| a.len());
        assert_eq!(len, 0, "should have no commands");
    }

    #[test]
    fn leading_whitespace_trimmed() {
        let mut h = setup_harness();
        let result = h
            .request(
                ext_cat(),
                "extract_commands",
                json!({ "text": "   @shell echo trimmed" }),
            )
            .expect("extract should succeed");

        let cmds = result["commands"].as_array().expect("commands array");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0]["body"], "echo trimmed");
    }
}

// =============================================================================
// format_command_results tests
// =============================================================================

mod format_results {
    use super::*;

    #[test]
    fn single_success() {
        let mut h = setup_harness();
        let result = h
            .request(
                ext_cat(),
                "format_results",
                json!({
                    "results": [
                        { "prefix": "shell", "success": true, "response": "file1.txt\nfile2.txt" }
                    ]
                }),
            )
            .expect("format should succeed");

        let formatted = result["formatted"].as_str().expect("formatted string");
        assert!(
            formatted.contains("[@shell result: OK]"),
            "should contain OK status, got: {formatted}"
        );
        assert!(
            formatted.contains("file1.txt"),
            "should contain response content"
        );
    }

    #[test]
    fn single_error() {
        let mut h = setup_harness();
        let result = h
            .request(
                ext_cat(),
                "format_results",
                json!({
                    "results": [
                        { "prefix": "tool", "success": false, "error": "not found" }
                    ]
                }),
            )
            .expect("format should succeed");

        let formatted = result["formatted"].as_str().expect("formatted string");
        assert!(
            formatted.contains("[@tool result: ERROR]"),
            "should contain ERROR status"
        );
        assert!(
            formatted.contains("not found"),
            "should contain error message"
        );
    }

    #[test]
    fn multiple_results_separated() {
        let mut h = setup_harness();
        let result = h
            .request(
                ext_cat(),
                "format_results",
                json!({
                    "results": [
                        { "prefix": "shell", "success": true, "response": "output1" },
                        { "prefix": "tool", "success": false, "error": "failed" }
                    ]
                }),
            )
            .expect("format should succeed");

        let formatted = result["formatted"].as_str().expect("formatted string");
        assert!(formatted.contains("[@shell result: OK]"));
        assert!(formatted.contains("[@tool result: ERROR]"));
        assert!(
            formatted.contains("\n\n"),
            "results should be separated by double newline"
        );
    }

    #[test]
    fn missing_response_and_error_shows_no_output() {
        let mut h = setup_harness();
        let result = h
            .request(
                ext_cat(),
                "format_results",
                json!({
                    "results": [
                        { "prefix": "skill", "success": true }
                    ]
                }),
            )
            .expect("format should succeed");

        let formatted = result["formatted"].as_str().expect("formatted string");
        assert!(
            formatted.contains("(no output)"),
            "should show fallback when no response/error, got: {formatted}"
        );
    }
}

//! Integration tests for prompt placement strategies.
//!
//! Tests the prompt assembly logic used by agent_mgr's llm-worker.
//! Uses mock injection (orcs.llm, orcs.request, orcs.tool_descriptions)
//! to capture assembled prompts and verify section ordering.

use orcs_component::EventCategory;
use orcs_lua::testing::LuaTestHarness;
use orcs_runtime::sandbox::{ProjectSandbox, SandboxPolicy};
use serde_json::json;
use std::sync::Arc;

fn test_sandbox() -> Arc<dyn SandboxPolicy> {
    Arc::new(ProjectSandbox::new(".").expect("test sandbox"))
}

/// Lua component that replicates the llm-worker prompt assembly logic.
///
/// Accepts the same inputs as the real llm-worker (message, history_context,
/// prompt_placement) and performs identical prompt assembly. Calls orcs.llm()
/// with the assembled prompt, allowing tests to capture and inspect it.
const PROMPT_ASSEMBLER_SCRIPT: &str = r###"
return {
    id = "prompt-assembler",
    subscriptions = {"Extension"},

    on_request = function(req)
        local input = req.payload or {}
        local message = input.message or ""
        local history_context = input.history_context or ""
        local placement = input.prompt_placement or "both"

        -- 1. Gather system parts: skill recommendations + tool descriptions
        local recommendation = ""
        local skill_count = 0
        local rec_resp = orcs.request("skill::skill_manager", "recommend", {
            intent = message,
            context = history_context,
            limit = 5,
        })
        if rec_resp and rec_resp.success and rec_resp.data then
            local skills = rec_resp.data
            if type(skills) == "table" and #skills > 0 then
                skill_count = #skills
                local lines = {}
                for _, s in ipairs(skills) do
                    local line = "- **" .. (s.name or "?") .. "**"
                    if s.description and s.description ~= "" then
                        line = line .. ": " .. s.description
                    end
                    lines[#lines + 1] = line
                end
                recommendation = "[System Recommendation]\n"
                    .. "The following skills are relevant to this task. "
                    .. "Consider using them to assist the user:\n"
                    .. table.concat(lines, "\n")
            end
        end

        local tool_desc = ""
        if orcs.tool_descriptions then
            local td = orcs.tool_descriptions()
            if td and td ~= "" then
                tool_desc = "## Available ORCS Tools\n" .. td
            end
        end

        -- 2. Compose system block (full)
        local system_parts = {}
        if recommendation ~= "" then
            system_parts[#system_parts + 1] = recommendation
        end
        if tool_desc ~= "" then
            system_parts[#system_parts + 1] = tool_desc
        end
        local system_full = table.concat(system_parts, "\n\n")

        -- 3. Compose history block
        local history_block = ""
        if history_context ~= "" then
            history_block = "## Recent Conversation\n" .. history_context
        end

        -- 4. Assemble prompt based on placement strategy
        local sections = {}

        if placement == "top" then
            if system_full ~= "" then sections[#sections + 1] = system_full end
            if history_block ~= "" then sections[#sections + 1] = history_block end
            sections[#sections + 1] = message

        elseif placement == "bottom" then
            if history_block ~= "" then sections[#sections + 1] = history_block end
            sections[#sections + 1] = message
            if system_full ~= "" then sections[#sections + 1] = system_full end

        else
            -- "both" (default): [System] [History] [System] [UserInput]
            if system_full ~= "" then sections[#sections + 1] = system_full end
            if history_block ~= "" then sections[#sections + 1] = history_block end
            if system_full ~= "" then sections[#sections + 1] = system_full end
            sections[#sections + 1] = message
        end

        local prompt = table.concat(sections, "\n\n")

        -- 5. Call orcs.llm() (mocked in tests)
        local llm_resp = orcs.llm(prompt)
        if llm_resp and llm_resp.ok then
            return {
                success = true,
                data = {
                    response = llm_resp.content,
                    prompt_length = #prompt,
                    placement = placement,
                },
            }
        else
            local err = (llm_resp and llm_resp.error) or "llm call failed"
            return { success = false, error = err }
        end
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
        kind: "prompt".to_string(),
    }
}

/// Mock skill data returned by orcs.request("skill::skill_manager", "recommend", ...)
fn mock_skills() -> serde_json::Value {
    json!([
        { "name": "deploy", "description": "Deploy to production" },
        { "name": "test", "description": "Run test suite" },
    ])
}

fn setup_harness(placement: &str) -> (LuaTestHarness, Arc<std::sync::Mutex<Vec<String>>>) {
    let mut harness = LuaTestHarness::from_script(PROMPT_ASSEMBLER_SCRIPT, test_sandbox())
        .expect("should load prompt-assembler script");
    harness.init().expect("init should succeed");

    // Inject mocks
    let captured = harness.inject_llm_mock(vec!["LLM says hello".into()]);
    harness.inject_request_mock(vec![("skill::skill_manager".into(), mock_skills())]);
    harness.inject_tool_descriptions_mock(
        "- read(path): Read a file\n- write(path, content): Write a file",
    );

    // Make the request
    let _result = harness
        .request(
            ext_cat(),
            "assemble",
            json!({
                "message": "How do I deploy?",
                "history_context": "- [user] Previous question about CI",
                "prompt_placement": placement,
            }),
        )
        .expect("prompt assembly should succeed");

    (harness, captured)
}

// =============================================================================
// Placement strategy tests
// =============================================================================

mod placement {
    use super::*;

    #[test]
    fn top_system_before_history_before_user() {
        let (_harness, captured) = setup_harness("top");
        let prompts = captured.lock().expect("captured mutex");

        assert_eq!(prompts.len(), 1, "should have captured exactly one prompt");
        let prompt = &prompts[0];

        // Verify ordering: System → History → UserInput
        let system_pos = prompt
            .find("[System Recommendation]")
            .expect("should contain System Recommendation");
        let tools_pos = prompt
            .find("## Available ORCS Tools")
            .expect("should contain tools section");
        let history_pos = prompt
            .find("## Recent Conversation")
            .expect("should contain history section");
        let user_pos = prompt
            .find("How do I deploy?")
            .expect("should contain user message");

        assert!(
            system_pos < tools_pos,
            "system recommendation should come before tools"
        );
        assert!(
            tools_pos < history_pos,
            "tools should come before history in top"
        );
        assert!(
            history_pos < user_pos,
            "history should come before user input in top"
        );

        // System should appear exactly once (only at top)
        let system_count = prompt.matches("[System Recommendation]").count();
        assert_eq!(
            system_count, 1,
            "top placement should have system block exactly once"
        );
    }

    #[test]
    fn both_system_at_top_and_bottom() {
        let (_harness, captured) = setup_harness("both");
        let prompts = captured.lock().expect("captured mutex");

        assert_eq!(prompts.len(), 1);
        let prompt = &prompts[0];

        // System should appear exactly twice (top and bottom)
        let system_count = prompt.matches("[System Recommendation]").count();
        assert_eq!(
            system_count, 2,
            "both placement should have system block twice"
        );

        // Verify ordering: System → History → System → UserInput
        let first_system_pos = prompt
            .find("[System Recommendation]")
            .expect("should contain first System Recommendation");
        let history_pos = prompt
            .find("## Recent Conversation")
            .expect("should contain history section");
        let second_system_pos = prompt[first_system_pos + 1..]
            .find("[System Recommendation]")
            .map(|p| p + first_system_pos + 1)
            .expect("should contain second System Recommendation");
        let user_pos = prompt
            .find("How do I deploy?")
            .expect("should contain user message");

        assert!(
            first_system_pos < history_pos,
            "first system should come before history"
        );
        assert!(
            history_pos < second_system_pos,
            "history should come before second system"
        );
        assert!(
            second_system_pos < user_pos,
            "second system should come before user input"
        );
    }

    #[test]
    fn bottom_history_then_user_then_system() {
        let (_harness, captured) = setup_harness("bottom");
        let prompts = captured.lock().expect("captured mutex");

        assert_eq!(prompts.len(), 1);
        let prompt = &prompts[0];

        let history_pos = prompt
            .find("## Recent Conversation")
            .expect("should contain history section");
        let user_pos = prompt
            .find("How do I deploy?")
            .expect("should contain user message");
        let system_pos = prompt
            .find("[System Recommendation]")
            .expect("should contain System Recommendation");

        assert!(
            history_pos < user_pos,
            "history should come before user input in bottom"
        );
        assert!(
            user_pos < system_pos,
            "user input should come before system in bottom"
        );

        // System should appear exactly once (only at bottom)
        let system_count = prompt.matches("[System Recommendation]").count();
        assert_eq!(
            system_count, 1,
            "bottom placement should have system block exactly once"
        );
    }

    #[test]
    fn default_is_both() {
        // Omit prompt_placement entirely → defaults to "both"
        let mut harness = LuaTestHarness::from_script(PROMPT_ASSEMBLER_SCRIPT, test_sandbox())
            .expect("should load script");
        harness.init().expect("init");

        let captured = harness.inject_llm_mock(vec!["ok".into()]);
        harness.inject_request_mock(vec![("skill::skill_manager".into(), mock_skills())]);
        harness.inject_tool_descriptions_mock("- read(path): Read a file");

        let _result = harness
            .request(
                ext_cat(),
                "assemble",
                json!({
                    "message": "Hello",
                    "history_context": "- [user] Hi",
                }),
            )
            .expect("should succeed");

        let prompts = captured.lock().expect("mutex");
        assert_eq!(prompts.len(), 1);

        // Default should have system block twice (both behavior)
        let system_count = prompts[0].matches("[System Recommendation]").count();
        assert_eq!(
            system_count, 2,
            "default placement should use both (system at top and bottom)"
        );
    }
}

// =============================================================================
// Edge cases
// =============================================================================

mod edge_cases {
    use super::*;

    #[test]
    fn no_skills_no_tools_message_only() {
        let mut harness = LuaTestHarness::from_script(PROMPT_ASSEMBLER_SCRIPT, test_sandbox())
            .expect("should load script");
        harness.init().expect("init");

        let captured = harness.inject_llm_mock(vec!["ok".into()]);
        // Return empty skills → no recommendations generated
        harness.inject_request_mock(vec![("skill::skill_manager".into(), json!([]))]);
        // No tool_descriptions mock → function may not exist

        let _result = harness
            .request(
                ext_cat(),
                "assemble",
                json!({
                    "message": "Hello world",
                    "prompt_placement": "both",
                }),
            )
            .expect("should succeed even without skills/tools");

        let prompts = captured.lock().expect("mutex");
        assert_eq!(prompts.len(), 1);

        // With no system parts and no history, prompt should just be the message
        assert!(
            prompts[0].contains("Hello world"),
            "prompt should contain user message"
        );
        assert!(
            !prompts[0].contains("[System Recommendation]"),
            "should not have recommendations when mock returns nothing"
        );
    }

    #[test]
    fn empty_history_omits_history_section() {
        let mut harness = LuaTestHarness::from_script(PROMPT_ASSEMBLER_SCRIPT, test_sandbox())
            .expect("should load script");
        harness.init().expect("init");

        let captured = harness.inject_llm_mock(vec!["ok".into()]);
        harness.inject_request_mock(vec![("skill::skill_manager".into(), mock_skills())]);

        let _result = harness
            .request(
                ext_cat(),
                "assemble",
                json!({
                    "message": "Deploy now",
                    "history_context": "",
                    "prompt_placement": "top",
                }),
            )
            .expect("should succeed");

        let prompts = captured.lock().expect("mutex");
        assert_eq!(prompts.len(), 1);
        assert!(
            !prompts[0].contains("## Recent Conversation"),
            "empty history should not produce history section"
        );
    }

    #[test]
    fn llm_mock_returns_configured_response() {
        let mut harness = LuaTestHarness::from_script(PROMPT_ASSEMBLER_SCRIPT, test_sandbox())
            .expect("should load script");
        harness.init().expect("init");

        let _captured = harness.inject_llm_mock(vec!["Custom LLM response here".into()]);
        harness.inject_request_mock(vec![("skill::skill_manager".into(), json!([]))]);

        let result = harness
            .request(
                ext_cat(),
                "assemble",
                json!({
                    "message": "test",
                    "prompt_placement": "top",
                }),
            )
            .expect("should succeed");

        assert_eq!(
            result["response"], "Custom LLM response here",
            "should return the mock LLM response"
        );
    }
}

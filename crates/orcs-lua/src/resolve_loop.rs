//! `orcs.resolve_loop(backend_fn, opts)` — provider-agnostic tool resolution loop.
//!
//! Exposes a Lua API that runs an iterative tool-resolution loop:
//! 1. Call `backend_fn(turn)` to get an LLM response
//! 2. If response contains `tool_calls`, dispatch them via `dispatch_intents_to_results()`
//! 3. Feed tool results back to `backend_fn` on the next turn
//! 4. Repeat until no `tool_calls` or `max_turns` reached
//!
//! This allows any custom LLM provider (HTTP API, CLI wrapper, local model)
//! to leverage the built-in IntentRegistry and tool dispatch infrastructure
//! without depending on `orcs.llm()`.

use crate::llm_command::resolve::dispatch_intents_to_results;
use crate::tool_registry::IntentRegistry;
use crate::types::serde_json_to_lua;
use mlua::{Function, Lua, Table, Value};
use orcs_types::intent::{ActionIntent, ContentBlock, IntentMeta, MessageContent};

/// Default maximum number of tool resolution turns.
const DEFAULT_MAX_TURNS: u32 = 10;

/// Hard ceiling for max_turns to prevent runaway loops.
const MAX_TURNS_LIMIT: u32 = 100;

/// Register `orcs.resolve_loop` on the given orcs table.
pub fn register_resolve_loop(lua: &Lua, orcs_table: &Table) -> mlua::Result<()> {
    let resolve_loop_fn = lua.create_function(|lua, (backend_fn, opts): (Function, Table)| {
        resolve_loop_impl(lua, &backend_fn, &opts)
    })?;
    orcs_table.set("resolve_loop", resolve_loop_fn)?;
    Ok(())
}

/// Core implementation of `orcs.resolve_loop(backend_fn, opts)`.
///
/// ## Parameters
///
/// - `backend_fn`: `function(turn) -> response`
///   - `turn.prompt` (string): initial user message (turn 0 only)
///   - `turn.tool_results` (table[]): previous tool results (turn > 0)
///   - `turn.tools` (table[]): available tool definitions (snapshot at call time)
///   - `turn.turn_number` (number): current turn (0-indexed)
///   - Response: `{ content: string, tool_calls: table[]|nil }`
///     - `tool_calls[i]`: `{ id: string, name: string, arguments: table }`
///
/// - `opts`:
///   - `prompt` (string, required): initial user message
///   - `max_turns` (number, optional, default 10, capped at 100):
///     Maximum number of tool dispatch rounds. `backend_fn` is called up to
///     `max_turns + 1` times (N dispatches + 1 final response).
///     `max_turns = 0` means tool resolution is disabled (single call only).
///
/// ## Returns
///
/// `{ ok: bool, content: string, num_turns: number, error: string? }`
fn resolve_loop_impl(lua: &Lua, backend_fn: &Function, opts: &Table) -> mlua::Result<Table> {
    let prompt: String = opts.get("prompt").map_err(|_| {
        mlua::Error::RuntimeError("resolve_loop: 'prompt' is required in opts".into())
    })?;
    let max_turns: u32 = opts
        .get("max_turns")
        .unwrap_or(DEFAULT_MAX_TURNS)
        .min(MAX_TURNS_LIMIT);

    // Snapshot tools from IntentRegistry once. The Lua table is a shared
    // reference — backend_fn mutations to it will persist across turns.
    let tools_table = build_tools_table(lua)?;

    let mut last_content = String::new();
    let mut tool_results_content: Option<MessageContent> = None;
    let mut turn_number = 0u32;

    loop {
        let turn = build_turn_table(
            lua,
            turn_number,
            &prompt,
            &tools_table,
            &tool_results_content,
        )?;

        let response = match backend_fn.call::<Table>(turn) {
            Ok(resp) => resp,
            Err(e) => {
                // Intent-level permission denial: surface as a resolve_loop
                // error result so the caller (Lua) can handle gracefully
                // (e.g., the LLM decides to delegate to an agent).
                if let Some((grant_pattern, description)) = crate::extract_suspended_info(&e) {
                    return build_result(
                        lua,
                        false,
                        &last_content,
                        turn_number,
                        Some(format!(
                            "Permission denied: {description}. \
                             Missing '{grant_pattern}' permission."
                        )),
                    );
                }
                return build_result(
                    lua,
                    false,
                    &last_content,
                    turn_number,
                    Some(format!("backend_fn error: {e}")),
                );
            }
        };

        last_content = response.get::<String>("content").unwrap_or_default();
        let intents = parse_response_intents(lua, &response)?;

        if intents.is_empty() {
            return build_result(lua, true, &last_content, turn_number, None);
        }
        if turn_number >= max_turns {
            return build_result(
                lua,
                false,
                &last_content,
                turn_number,
                Some(format!("resolve_loop exceeded max_turns ({max_turns})")),
            );
        }

        // resolve_loop has no session management, so always hil_intents=false:
        // Suspended is converted to an error tool_result instead of propagating.
        tool_results_content = Some(dispatch_intents_to_results(lua, &intents, false)?);
        log_dispatch(turn_number, &intents);
        turn_number += 1;
    }
}

/// Build the turn table passed to `backend_fn` on each iteration.
fn build_turn_table(
    lua: &Lua,
    turn_number: u32,
    prompt: &str,
    tools_table: &Table,
    tool_results_content: &Option<MessageContent>,
) -> mlua::Result<Table> {
    let turn = lua.create_table()?;
    turn.set("turn_number", turn_number)?;
    // Shared Lua table reference (ref-count copy, not deep clone).
    turn.set("tools", tools_table.clone())?;

    if turn_number == 0 {
        turn.set("prompt", prompt)?;
    }
    if let Some(ref results) = tool_results_content {
        let results_lua = content_blocks_to_lua_results(lua, results)?;
        turn.set("tool_results", results_lua)?;
    }
    Ok(turn)
}

/// Extract tool_calls from a backend_fn response into ActionIntents.
fn parse_response_intents(lua: &Lua, response: &Table) -> mlua::Result<Vec<ActionIntent>> {
    let tool_calls_value: Value = response.get("tool_calls").unwrap_or(Value::Nil);
    match tool_calls_value {
        Value::Table(ref tc_table) => parse_tool_calls_from_lua(lua, tc_table),
        _ => Ok(vec![]),
    }
}

/// Log dispatched intents with structured tracing fields.
fn log_dispatch(turn_number: u32, intents: &[ActionIntent]) {
    let intent_names: Vec<&str> = intents.iter().map(|i| i.name.as_str()).collect();
    tracing::info!(
        turn = turn_number,
        count = intents.len(),
        intents = %intent_names.join(", "),
        "resolve_loop dispatched intents"
    );
}

/// Build the tools table from IntentRegistry for passing to backend_fn.
///
/// Each tool entry: `{ name, description, parameters }` where parameters
/// is a JSON Schema object converted to a Lua table.
fn build_tools_table(lua: &Lua) -> mlua::Result<Table> {
    let tools = lua.create_table()?;

    let registry = lua
        .app_data_ref::<IntentRegistry>()
        .ok_or_else(|| mlua::Error::RuntimeError("IntentRegistry not initialized".into()))?;

    for (i, def) in registry.all().iter().enumerate() {
        let entry = lua.create_table()?;
        entry.set("name", def.name.as_str())?;
        entry.set("description", def.description.as_str())?;

        let params_lua = serde_json_to_lua(&def.parameters, lua)?;
        entry.set("parameters", params_lua)?;

        tools.set(i + 1, entry)?;
    }

    Ok(tools)
}

/// Parse tool_calls Lua table into ActionIntents.
///
/// Expected format: array of `{ id: string, name: string, arguments: table }`
fn parse_tool_calls_from_lua(lua: &Lua, table: &Table) -> mlua::Result<Vec<ActionIntent>> {
    let mut intents = Vec::new();

    for pair in table.clone().pairs::<i64, Table>() {
        let (_, tc) = pair?;

        let name: String = tc.get("name").map_err(|_| {
            mlua::Error::RuntimeError("resolve_loop: tool_call missing 'name' field".into())
        })?;

        // id is optional — generate UUID v4 if missing (matches codebase-wide convention)
        let id: String = tc
            .get("id")
            .unwrap_or_else(|_| format!("tc_{}", uuid::Uuid::new_v4()));

        // arguments can be a table or nil
        let arguments_value: Value = tc.get("arguments").unwrap_or(Value::Nil);
        let params = match arguments_value {
            Value::Table(ref t) => crate::types::lua_to_json(Value::Table(t.clone()), lua)?,
            Value::Nil => serde_json::Value::Object(serde_json::Map::new()),
            other => {
                return Err(mlua::Error::RuntimeError(format!(
                    "resolve_loop: tool_call '{}' arguments must be a table, got: {:?}",
                    name, other
                )));
            }
        };

        intents.push(ActionIntent {
            id,
            name,
            params,
            meta: IntentMeta::default(),
        });
    }

    Ok(intents)
}

/// Convert `MessageContent::Blocks` (ToolResult blocks) to a Lua array for backend_fn.
///
/// Each entry: `{ tool_call_id: string, content: string, is_error: bool }`
fn content_blocks_to_lua_results(lua: &Lua, content: &MessageContent) -> mlua::Result<Table> {
    let results = lua.create_table()?;

    let blocks = match content {
        MessageContent::Blocks(b) => b,
        _ => return Ok(results),
    };

    for (i, block) in blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some((tool_use_id, content, is_error)),
            _ => None,
        })
        .enumerate()
    {
        let (tool_use_id, content, is_error) = block;
        let entry = lua.create_table()?;
        entry.set("tool_call_id", tool_use_id.as_str())?;
        entry.set("content", content.as_str())?;
        entry.set("is_error", is_error.unwrap_or(false))?;
        results.set(i + 1, entry)?;
    }

    Ok(results)
}

/// Build the final result table returned by `orcs.resolve_loop`.
fn build_result(
    lua: &Lua,
    ok: bool,
    content: &str,
    num_turns: u32,
    error: Option<String>,
) -> mlua::Result<Table> {
    let result = lua.create_table()?;
    result.set("ok", ok)?;
    result.set("content", content)?;
    result.set("num_turns", num_turns)?;
    if let Some(e) = error {
        result.set("error", e)?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orcs_helpers::register_base_orcs_functions;
    use orcs_runtime::sandbox::{ProjectSandbox, SandboxPolicy};
    use orcs_runtime::WorkDir;
    use std::sync::Arc;

    fn test_sandbox() -> (WorkDir, Arc<dyn SandboxPolicy>) {
        let wd = WorkDir::temporary().expect("should create temp work dir");
        let dir = wd
            .path()
            .canonicalize()
            .expect("should canonicalize temp dir");
        let sandbox = ProjectSandbox::new(&dir).expect("test sandbox");
        (wd, Arc::new(sandbox))
    }

    fn setup_lua(sandbox: Arc<dyn SandboxPolicy>) -> Lua {
        let lua = Lua::new();
        register_base_orcs_functions(&lua, sandbox).expect("should register base functions");

        // Register resolve_loop
        let orcs: Table = lua.globals().get("orcs").expect("orcs table should exist");
        register_resolve_loop(&lua, &orcs).expect("should register resolve_loop");

        lua
    }

    #[test]
    fn single_turn_no_tool_calls() {
        let (_wd, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let result: Table = lua
            .load(
                r#"
                local result = orcs.resolve_loop(function(turn)
                    return {
                        content = "Hello from backend",
                        tool_calls = nil,
                    }
                end, {
                    prompt = "Say hello",
                })
                return result
                "#,
            )
            .eval()
            .expect("resolve_loop should succeed");

        assert!(
            result.get::<bool>("ok").expect("should have ok field"),
            "should succeed"
        );
        assert_eq!(
            result
                .get::<String>("content")
                .expect("should have content"),
            "Hello from backend"
        );
        assert_eq!(
            result
                .get::<u32>("num_turns")
                .expect("should have num_turns"),
            0,
            "single turn completes at turn 0"
        );
    }

    #[test]
    fn multi_turn_with_tool_calls() {
        let (wd, sandbox) = test_sandbox();

        // Write a test file for the read tool
        std::fs::write(wd.path().join("test.txt"), "file content here")
            .expect("should write test file");

        let lua = setup_lua(sandbox);

        // Backend that returns a read tool_call on first turn, then final response
        let code = format!(
            r#"
            local result = orcs.resolve_loop(function(turn)
                if turn.turn_number == 0 then
                    return {{
                        content = "Let me read that file.",
                        tool_calls = {{{{
                            id = "call_1",
                            name = "read",
                            arguments = {{ path = "{path}" }},
                        }}}},
                    }}
                else
                    -- turn 1: we have tool_results
                    local file_content = ""
                    if turn.tool_results then
                        for _, tr in ipairs(turn.tool_results) do
                            file_content = file_content .. tr.content
                        end
                    end
                    return {{
                        content = "File contains: " .. file_content,
                        tool_calls = nil,
                    }}
                end
            end, {{
                prompt = "Read test.txt",
            }})
            return result
            "#,
            path = wd.path().join("test.txt").display()
        );

        let result: Table = lua.load(&code).eval().expect("resolve_loop should succeed");

        assert!(
            result.get::<bool>("ok").expect("should have ok"),
            "should succeed"
        );
        let content = result
            .get::<String>("content")
            .expect("should have content");
        assert!(
            content.contains("file content here") || content.contains("file_content"),
            "final content should reference file content, got: {content}"
        );
        assert_eq!(
            result
                .get::<u32>("num_turns")
                .expect("should have num_turns"),
            1,
            "should complete on turn 1 (after dispatching on turn 0)"
        );
    }

    #[test]
    fn max_turns_exceeded() {
        let (_wd, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let result: Table = lua
            .load(
                r#"
                local result = orcs.resolve_loop(function(turn)
                    -- Always return tool_calls to force loop
                    return {
                        content = "still going",
                        tool_calls = {{
                            id = "call_" .. turn.turn_number,
                            name = "read",
                            arguments = { path = "/nonexistent" },
                        }},
                    }
                end, {
                    prompt = "loop forever",
                    max_turns = 2,
                })
                return result
                "#,
            )
            .eval()
            .expect("resolve_loop should return result");

        assert!(
            !result.get::<bool>("ok").expect("should have ok"),
            "should fail on max_turns"
        );
        let error = result.get::<String>("error").expect("should have error");
        assert!(
            error.contains("max_turns"),
            "error should mention max_turns, got: {error}"
        );
    }

    #[test]
    fn backend_error_returns_error() {
        let (_wd, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let result: Table = lua
            .load(
                r#"
                local result = orcs.resolve_loop(function(turn)
                    error("backend exploded")
                end, {
                    prompt = "trigger error",
                })
                return result
                "#,
            )
            .eval()
            .expect("resolve_loop should return result even on backend error");

        assert!(
            !result.get::<bool>("ok").expect("should have ok"),
            "should report failure"
        );
        let error = result.get::<String>("error").expect("should have error");
        assert!(
            error.contains("backend_fn error"),
            "should wrap backend error, got: {error}"
        );
    }

    #[test]
    fn missing_prompt_errors() {
        let (_wd, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let result = lua
            .load(
                r#"
                return orcs.resolve_loop(function(turn)
                    return { content = "hi" }
                end, {})
                "#,
            )
            .eval::<Table>();

        assert!(result.is_err(), "should error when prompt is missing");
        let err = result.expect_err("should error").to_string();
        assert!(
            err.contains("prompt"),
            "error should mention 'prompt', got: {err}"
        );
    }

    #[test]
    fn tools_table_contains_registry_entries() {
        let (_wd, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        // Verify tools are passed to backend_fn
        let result: Table = lua
            .load(
                r#"
                local captured_tools = nil
                local result = orcs.resolve_loop(function(turn)
                    captured_tools = turn.tools
                    return { content = "done" }
                end, {
                    prompt = "check tools",
                })
                -- Return number of tools
                local count = 0
                for _ in ipairs(captured_tools) do count = count + 1 end
                result.tool_count = count
                result.first_tool_name = captured_tools[1] and captured_tools[1].name or "none"
                return result
                "#,
            )
            .eval()
            .expect("resolve_loop should succeed");

        let tool_count: u32 = result.get("tool_count").expect("should have tool_count");
        assert!(
            tool_count >= 8,
            "should have at least 8 builtin tools, got: {tool_count}"
        );
        assert_eq!(
            result
                .get::<String>("first_tool_name")
                .expect("should have first_tool_name"),
            "read"
        );
    }

    #[test]
    fn tool_call_without_id_gets_auto_generated() {
        let (_wd, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let result: Table = lua
            .load(
                r#"
                local result = orcs.resolve_loop(function(turn)
                    if turn.turn_number == 0 then
                        return {
                            content = "reading",
                            tool_calls = {{
                                name = "read",
                                arguments = { path = "/nonexistent" },
                            }},
                        }
                    end
                    -- Check that tool_results have a tool_call_id
                    local has_id = false
                    if turn.tool_results and turn.tool_results[1] then
                        has_id = turn.tool_results[1].tool_call_id ~= nil
                            and #turn.tool_results[1].tool_call_id > 0
                    end
                    return { content = tostring(has_id) }
                end, {
                    prompt = "test auto id",
                })
                return result
                "#,
            )
            .eval()
            .expect("resolve_loop should succeed");

        assert!(
            result.get::<bool>("ok").expect("should have ok"),
            "should succeed"
        );
        assert_eq!(
            result
                .get::<String>("content")
                .expect("should have content"),
            "true",
            "auto-generated tool_call_id should propagate to tool_results"
        );
    }

    #[test]
    fn dynamically_registered_intent_available_in_tools() {
        let (_wd, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let result: Table = lua
            .load(
                r#"
                -- Register a custom intent before resolve_loop
                orcs.register_intent({
                    name = "my_custom_tool",
                    description = "A custom tool for testing",
                    component = "lua::test",
                    operation = "execute",
                    params = {
                        input = { type = "string", description = "Input data", required = true },
                    },
                })

                local found_custom = false
                local result = orcs.resolve_loop(function(turn)
                    for _, tool in ipairs(turn.tools) do
                        if tool.name == "my_custom_tool" then
                            found_custom = true
                        end
                    end
                    return { content = tostring(found_custom) }
                end, {
                    prompt = "check custom tool",
                })
                return result
                "#,
            )
            .eval()
            .expect("resolve_loop should succeed");

        assert!(
            result.get::<bool>("ok").expect("should have ok"),
            "should succeed"
        );
        assert_eq!(
            result
                .get::<String>("content")
                .expect("should have content"),
            "true",
            "dynamically registered intent should appear in tools table"
        );
    }
}

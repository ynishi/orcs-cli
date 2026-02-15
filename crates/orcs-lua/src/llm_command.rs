//! Lua-driven LLM handler for `orcs.llm()`.
//!
//! LLM invocation is fully controlled by a Lua handler function registered
//! via `orcs.set_llm_handler(fn)`. A default handler (Claude Code CLI) is
//! installed automatically but can be replaced by any Lua function.
//!
//! ```text
//! Lua: orcs.llm(prompt, opts)
//!   → Capability::LLM gate (ctx_fns / child)
//!   → call registered Lua handler(prompt, opts)
//!       ├── default: Claude Code CLI via orcs.exec
//!       └── custom:  user-defined (OpenAI, Ollama, etc.)
//! ```

use crate::error::LuaError;
use mlua::{Lua, RegistryKey, Table};

/// Lua registry key for the LLM handler function.
struct LlmHandlerKey(RegistryKey);

/// Default LLM handler: spawns `claude -p --output-format json` via `orcs.exec`.
///
/// Returns structured response parsed from Claude Code CLI JSON output:
///   `{ ok, content, session_id, num_turns, cost, subtype }`
///
/// When `opts.session_id` is set, resumes a previous session via `--resume`.
///
/// Supports opts: `model`, `session_id`, `system_prompt`, `max_turns`.
/// Removes Claude Code nested-session guard env vars before execution.
const DEFAULT_HANDLER_LUA: &str = r#"
local function shell_escape(s)
    if s == nil then return "''" end
    return "'" .. tostring(s):gsub("'", "'\\''" ) .. "'"
end

return function(prompt, opts)
    opts = opts or {}

    local cmd_parts = {
        "unset CLAUDECODE CLAUDE_CODE_ENTRYPOINT CLAUDE_CODE_SSE_PORT 2>/dev/null;",
        "claude", "-p",
        "--output-format", "json",
    }

    if opts.model and opts.model ~= "" then
        cmd_parts[#cmd_parts + 1] = "--model"
        cmd_parts[#cmd_parts + 1] = shell_escape(opts.model)
    end

    if opts.session_id and opts.session_id ~= "" then
        cmd_parts[#cmd_parts + 1] = "--resume"
        cmd_parts[#cmd_parts + 1] = shell_escape(opts.session_id)
    end

    if opts.system_prompt and opts.system_prompt ~= "" then
        cmd_parts[#cmd_parts + 1] = "--system-prompt"
        cmd_parts[#cmd_parts + 1] = shell_escape(opts.system_prompt)
    end

    if opts.max_turns then
        cmd_parts[#cmd_parts + 1] = "--max-turns"
        cmd_parts[#cmd_parts + 1] = tostring(opts.max_turns)
    end

    cmd_parts[#cmd_parts + 1] = shell_escape(prompt)

    local cmd = table.concat(cmd_parts, " ")
    orcs.log("debug", "llm-handler: " .. cmd:sub(1, 300))

    local result = orcs.exec(cmd)

    if not result.ok then
        local err = result.stderr
        if err == "" then err = result.stdout end
        if err == "" then err = "claude CLI exited with code " .. tostring(result.code) end
        return { ok = false, error = err }
    end

    -- Parse JSON response from --output-format json
    local raw = result.stdout
    local parse_ok, parsed = pcall(orcs.json_parse, raw)
    if not parse_ok or not parsed then
        -- Fallback: treat raw stdout as plain text (pre-JSON handler compat)
        return { ok = true, content = raw }
    end

    if parsed.subtype == "success" then
        return {
            ok = true,
            content = parsed.result or "",
            session_id = parsed.session_id,
            num_turns = parsed.num_turns,
            cost = parsed.total_cost_usd,
            subtype = parsed.subtype,
        }
    else
        -- Error subtypes: error_max_budget_usd, error_tool, etc.
        local err = parsed.result or parsed.subtype or "unknown error"
        return {
            ok = false,
            error = err,
            session_id = parsed.session_id,
            cost = parsed.total_cost_usd,
            subtype = parsed.subtype,
        }
    end
end
"#;

/// Registers `orcs.set_llm_handler(fn)` and installs the default Claude CLI handler.
///
/// Call this from `register_base_orcs_functions` during Lua VM setup.
pub fn register_llm_functions(lua: &Lua, orcs_table: &Table) -> Result<(), LuaError> {
    // orcs.set_llm_handler(fn) — store handler in Lua registry
    let set_handler_fn = lua.create_function(|lua, handler: mlua::Function| {
        let key = lua.create_registry_value(handler)?;
        // Replace existing handler if present
        if let Some(old) = lua.remove_app_data::<LlmHandlerKey>() {
            lua.remove_registry_value(old.0)?;
        }
        lua.set_app_data(LlmHandlerKey(key));
        Ok(())
    })?;
    orcs_table.set("set_llm_handler", set_handler_fn)?;

    // Install default handler (Claude CLI)
    let default_handler: mlua::Function = lua
        .load(DEFAULT_HANDLER_LUA)
        .set_name("default_llm_handler")
        .eval()?;
    let key = lua.create_registry_value(default_handler)?;
    lua.set_app_data(LlmHandlerKey(key));

    Ok(())
}

/// Calls the registered LLM handler from Lua.
///
/// Used by `ctx_fns` and `child` to implement `orcs.llm()` after the
/// Capability::LLM gate check.
pub fn call_llm_handler(lua: &Lua, prompt: String, opts: Option<Table>) -> mlua::Result<Table> {
    let handler_key = lua.app_data_ref::<LlmHandlerKey>().ok_or_else(|| {
        mlua::Error::RuntimeError(
            "no LLM handler registered (call orcs.set_llm_handler first)".into(),
        )
    })?;

    let handler: mlua::Function = lua.registry_value(&handler_key.0)?;
    drop(handler_key);

    let result: Table = handler.call((prompt, opts))?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orcs_helpers::ensure_orcs_table;

    /// Register a no-op orcs.log and orcs.json_parse on the given orcs table.
    fn register_test_helpers(lua: &Lua, orcs: &Table) {
        let log_fn = lua
            .create_function(|_, (_level, _msg): (String, String)| Ok(()))
            .expect("create log fn");
        orcs.set("log", log_fn).expect("set log");

        // orcs.json_parse — needed by the default handler
        let json_parse_fn = lua
            .create_function(|lua, raw: String| {
                let val: serde_json::Value = serde_json::from_str(&raw)
                    .map_err(|_| mlua::Error::RuntimeError("bad json".into()))?;
                mlua::LuaSerdeExt::to_value(lua, &val)
            })
            .expect("create json_parse fn");
        orcs.set("json_parse", json_parse_fn)
            .expect("set json_parse");
    }

    /// Fake orcs.exec that returns raw stdout (caller controls JSON content).
    fn register_fake_exec_raw(lua: &Lua, orcs: &Table, stdout: &str) {
        let s = stdout.to_string();
        let exec_fn = lua
            .create_function(move |lua, _cmd: String| {
                let result = lua.create_table()?;
                result.set("ok", true)?;
                result.set("stdout", s.as_str())?;
                result.set("stderr", "")?;
                result.set("code", 0)?;
                Ok(result)
            })
            .expect("create exec fn");
        orcs.set("exec", exec_fn).expect("set exec");
    }

    /// Fake orcs.exec that captures the command and returns JSON.
    fn register_fake_exec_capture(lua: &Lua, orcs: &Table, json_response: &str) {
        let json_resp = json_response.to_string();
        let exec_fn = lua
            .create_function(move |lua, cmd: String| {
                // Store the command for later inspection
                let orcs_t: Table = lua.globals().get("orcs").expect("get orcs");
                orcs_t.set("_last_cmd", cmd).expect("set _last_cmd");
                let result = lua.create_table()?;
                result.set("ok", true)?;
                result.set("stdout", json_resp.as_str())?;
                result.set("stderr", "")?;
                result.set("code", 0)?;
                Ok(result)
            })
            .expect("create exec fn");
        orcs.set("exec", exec_fn).expect("set exec");
    }

    const FAKE_SUCCESS_JSON: &str = r#"{"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"Hello world.","session_id":"abc-123","total_cost_usd":0.01}"#;
    const FAKE_ERROR_JSON: &str = r#"{"type":"result","subtype":"error_max_budget_usd","is_error":false,"num_turns":0,"result":null,"session_id":"def-456","total_cost_usd":0.50}"#;

    #[test]
    fn set_llm_handler_and_call() {
        let lua = Lua::new();
        let orcs = ensure_orcs_table(&lua).expect("create orcs table");
        register_llm_functions(&lua, &orcs).expect("register llm functions");

        // Override with a simple echo handler
        let handler = lua
            .load(r#"function(prompt, opts) return { ok = true, content = "echo:" .. prompt } end"#)
            .eval::<mlua::Function>()
            .expect("create handler");

        let set_fn: mlua::Function = orcs.get("set_llm_handler").expect("get set_llm_handler");
        set_fn.call::<()>(handler).expect("set handler");

        let result = call_llm_handler(&lua, "hello".into(), None).expect("call handler");
        assert_eq!(result.get::<bool>("ok").expect("get ok"), true);
        assert_eq!(
            result.get::<String>("content").expect("get content"),
            "echo:hello"
        );
    }

    #[test]
    fn default_handler_parses_json_success() {
        let lua = Lua::new();
        let orcs = ensure_orcs_table(&lua).expect("get orcs table");
        register_test_helpers(&lua, &orcs);
        register_fake_exec_raw(&lua, &orcs, FAKE_SUCCESS_JSON);
        register_llm_functions(&lua, &orcs).expect("register llm functions");

        let result =
            call_llm_handler(&lua, "test prompt".into(), None).expect("call default handler");
        assert_eq!(result.get::<bool>("ok").expect("ok"), true);
        assert_eq!(
            result.get::<String>("content").expect("content"),
            "Hello world."
        );
        assert_eq!(
            result.get::<String>("session_id").expect("session_id"),
            "abc-123"
        );
        assert_eq!(result.get::<i64>("num_turns").expect("num_turns"), 1);
        assert_eq!(result.get::<String>("subtype").expect("subtype"), "success");
    }

    #[test]
    fn default_handler_parses_json_error_subtype() {
        let lua = Lua::new();
        let orcs = ensure_orcs_table(&lua).expect("get orcs table");
        register_test_helpers(&lua, &orcs);
        register_fake_exec_raw(&lua, &orcs, FAKE_ERROR_JSON);
        register_llm_functions(&lua, &orcs).expect("register llm functions");

        let result = call_llm_handler(&lua, "test".into(), None).expect("call handler");
        assert_eq!(result.get::<bool>("ok").expect("ok"), false);
        assert_eq!(
            result.get::<String>("subtype").expect("subtype"),
            "error_max_budget_usd"
        );
        assert_eq!(
            result.get::<String>("session_id").expect("session_id"),
            "def-456"
        );
    }

    #[test]
    fn default_handler_with_model_flag() {
        let lua = Lua::new();
        let orcs = ensure_orcs_table(&lua).expect("get orcs table");
        register_test_helpers(&lua, &orcs);
        register_fake_exec_capture(&lua, &orcs, FAKE_SUCCESS_JSON);
        register_llm_functions(&lua, &orcs).expect("register llm functions");

        let opts = lua.create_table().expect("create opts");
        opts.set("model", "claude-haiku-4-5-20251001")
            .expect("set model");

        call_llm_handler(&lua, "hi".into(), Some(opts)).expect("call with model");
        let cmd: String = lua
            .globals()
            .get::<Table>("orcs")
            .expect("orcs")
            .get("_last_cmd")
            .expect("_last_cmd");
        assert!(
            cmd.contains("--model") && cmd.contains("claude-haiku-4-5-20251001"),
            "should pass --model flag, got: {cmd}"
        );
    }

    #[test]
    fn default_handler_with_session_resume() {
        let lua = Lua::new();
        let orcs = ensure_orcs_table(&lua).expect("get orcs table");
        register_test_helpers(&lua, &orcs);
        register_fake_exec_capture(&lua, &orcs, FAKE_SUCCESS_JSON);
        register_llm_functions(&lua, &orcs).expect("register llm functions");

        let opts = lua.create_table().expect("create opts");
        opts.set("session_id", "prev-session-42")
            .expect("set session_id");

        call_llm_handler(&lua, "continue".into(), Some(opts)).expect("call with resume");
        let cmd: String = lua
            .globals()
            .get::<Table>("orcs")
            .expect("orcs")
            .get("_last_cmd")
            .expect("_last_cmd");
        assert!(
            cmd.contains("--resume") && cmd.contains("prev-session-42"),
            "should pass --resume flag, got: {cmd}"
        );
    }

    #[test]
    fn default_handler_includes_output_format_json() {
        let lua = Lua::new();
        let orcs = ensure_orcs_table(&lua).expect("get orcs table");
        register_test_helpers(&lua, &orcs);
        register_fake_exec_capture(&lua, &orcs, FAKE_SUCCESS_JSON);
        register_llm_functions(&lua, &orcs).expect("register llm functions");

        call_llm_handler(&lua, "hi".into(), None).expect("call handler");
        let cmd: String = lua
            .globals()
            .get::<Table>("orcs")
            .expect("orcs")
            .get("_last_cmd")
            .expect("_last_cmd");
        assert!(
            cmd.contains("--output-format") && cmd.contains("json"),
            "should include --output-format json, got: {cmd}"
        );
    }

    #[test]
    fn default_handler_fallback_plain_text() {
        let lua = Lua::new();
        let orcs = ensure_orcs_table(&lua).expect("get orcs table");
        register_test_helpers(&lua, &orcs);
        // Return non-JSON text (fallback path)
        register_fake_exec_raw(&lua, &orcs, "plain text response");
        register_llm_functions(&lua, &orcs).expect("register llm functions");

        let result = call_llm_handler(&lua, "test".into(), None).expect("call handler");
        assert_eq!(result.get::<bool>("ok").expect("ok"), true);
        assert_eq!(
            result.get::<String>("content").expect("content"),
            "plain text response"
        );
    }

    #[test]
    fn call_without_handler_errors() {
        let lua = Lua::new();
        let err = call_llm_handler(&lua, "test".into(), None);
        assert!(err.is_err(), "should error when no handler is registered");
    }

    #[test]
    fn set_handler_replaces_previous() {
        let lua = Lua::new();
        let orcs = ensure_orcs_table(&lua).expect("create orcs table");
        register_llm_functions(&lua, &orcs).expect("register llm functions");

        let set_fn: mlua::Function = orcs.get("set_llm_handler").expect("get set_llm_handler");

        // First handler returns "first"
        let h1 = lua
            .load(r#"function(p, o) return { ok = true, content = "first" } end"#)
            .eval::<mlua::Function>()
            .expect("create h1");
        set_fn.call::<()>(h1).expect("set h1");
        let r1 = call_llm_handler(&lua, "x".into(), None).expect("call h1");
        assert_eq!(r1.get::<String>("content").expect("get"), "first");

        // Second handler returns "second" — should replace
        let h2 = lua
            .load(r#"function(p, o) return { ok = true, content = "second" } end"#)
            .eval::<mlua::Function>()
            .expect("create h2");
        set_fn.call::<()>(h2).expect("set h2");
        let r2 = call_llm_handler(&lua, "x".into(), None).expect("call h2");
        assert_eq!(r2.get::<String>("content").expect("get"), "second");
    }

    #[test]
    fn default_handler_exec_failure() {
        let lua = Lua::new();
        let orcs = ensure_orcs_table(&lua).expect("create orcs table");
        register_test_helpers(&lua, &orcs);

        // Fake orcs.exec that always fails
        let exec_fn = lua
            .create_function(|lua, _cmd: String| {
                let result = lua.create_table()?;
                result.set("ok", false)?;
                result.set("stdout", "")?;
                result.set("stderr", "command not found: claude")?;
                result.set("code", 127)?;
                Ok(result)
            })
            .expect("create exec fn");
        orcs.set("exec", exec_fn).expect("set exec");

        register_llm_functions(&lua, &orcs).expect("register llm functions");

        let result = call_llm_handler(&lua, "test".into(), None).expect("call handler");
        assert_eq!(result.get::<bool>("ok").expect("get ok"), false);
        let err: String = result.get("error").expect("get error");
        assert!(
            err.contains("command not found"),
            "should contain error message, got: {}",
            err
        );
    }
}

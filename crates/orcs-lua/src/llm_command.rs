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

/// Default LLM handler: spawns `claude -p` via `orcs.exec`.
///
/// Supports opts: `model`, `new_session`, `resume`.
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
    }
    local session_id = nil

    if opts.model and opts.model ~= "" then
        cmd_parts[#cmd_parts + 1] = "--model"
        cmd_parts[#cmd_parts + 1] = shell_escape(opts.model)
    end

    if opts.resume and opts.resume ~= "" then
        cmd_parts[#cmd_parts + 1] = "--resume"
        cmd_parts[#cmd_parts + 1] = shell_escape(opts.resume)
    elseif opts.new_session then
        local uuid_result = orcs.exec("uuidgen 2>/dev/null || cat /proc/sys/kernel/random/uuid 2>/dev/null || echo fallback-" .. tostring(os.time()))
        if uuid_result.ok then
            session_id = uuid_result.stdout:gsub("%s+$", "")
        end
        if session_id and session_id ~= "" then
            cmd_parts[#cmd_parts + 1] = "--session-id"
            cmd_parts[#cmd_parts + 1] = shell_escape(session_id)
        end
    end

    cmd_parts[#cmd_parts + 1] = shell_escape(prompt)

    local cmd = table.concat(cmd_parts, " ")
    orcs.log("debug", "llm-handler: " .. cmd:sub(1, 200))

    local result = orcs.exec(cmd)

    if result.ok then
        return {
            ok = true,
            content = result.stdout,
            session_id = session_id,
        }
    else
        local err = result.stderr
        if err == "" then err = result.stdout end
        if err == "" then err = "claude CLI exited with code " .. tostring(result.code) end
        return { ok = false, error = err }
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

    /// Helper: set up a minimal Lua env with orcs table and a fake orcs.exec/orcs.log.
    fn setup_lua_with_fake_exec() -> Lua {
        let lua = Lua::new();
        let orcs = ensure_orcs_table(&lua).expect("create orcs table");

        // Fake orcs.log (no-op)
        let log_fn = lua
            .create_function(|_, (_level, _msg): (String, String)| Ok(()))
            .expect("create log fn");
        orcs.set("log", log_fn).expect("set log");

        // Fake orcs.exec that returns { ok=true, stdout=..., stderr="", code=0 }
        let exec_fn = lua
            .create_function(|lua, cmd: String| {
                let result = lua.create_table()?;
                if cmd.contains("uuidgen") {
                    result.set("ok", true)?;
                    result.set("stdout", "test-uuid-1234\n")?;
                    result.set("stderr", "")?;
                    result.set("code", 0)?;
                } else {
                    result.set("ok", true)?;
                    result.set("stdout", format!("response to: {}", cmd))?;
                    result.set("stderr", "")?;
                    result.set("code", 0)?;
                }
                Ok(result)
            })
            .expect("create exec fn");
        orcs.set("exec", exec_fn).expect("set exec");

        lua
    }

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
    fn default_handler_calls_claude_cli() {
        let lua = setup_lua_with_fake_exec();
        let orcs = ensure_orcs_table(&lua).expect("get orcs table");
        register_llm_functions(&lua, &orcs).expect("register llm functions");

        let result =
            call_llm_handler(&lua, "test prompt".into(), None).expect("call default handler");
        assert_eq!(result.get::<bool>("ok").expect("get ok"), true);
        let content: String = result.get("content").expect("get content");
        assert!(
            content.contains("claude") && content.contains("-p"),
            "default handler should call claude -p, got: {}",
            content
        );
    }

    #[test]
    fn default_handler_with_model() {
        let lua = setup_lua_with_fake_exec();
        let orcs = ensure_orcs_table(&lua).expect("get orcs table");
        register_llm_functions(&lua, &orcs).expect("register llm functions");

        let opts = lua.create_table().expect("create opts");
        opts.set("model", "claude-haiku-4-5-20251001")
            .expect("set model");

        let result = call_llm_handler(&lua, "hi".into(), Some(opts)).expect("call with model");
        let content: String = result.get("content").expect("get content");
        assert!(
            content.contains("--model") && content.contains("claude-haiku-4-5-20251001"),
            "should pass --model flag, got: {}",
            content
        );
    }

    #[test]
    fn default_handler_with_resume() {
        let lua = setup_lua_with_fake_exec();
        let orcs = ensure_orcs_table(&lua).expect("get orcs table");
        register_llm_functions(&lua, &orcs).expect("register llm functions");

        let opts = lua.create_table().expect("create opts");
        opts.set("resume", "abc-123").expect("set resume");

        let result =
            call_llm_handler(&lua, "continue".into(), Some(opts)).expect("call with resume");
        let content: String = result.get("content").expect("get content");
        assert!(
            content.contains("--resume") && content.contains("abc-123"),
            "should pass --resume flag, got: {}",
            content
        );
    }

    #[test]
    fn default_handler_with_new_session() {
        let lua = setup_lua_with_fake_exec();
        let orcs = ensure_orcs_table(&lua).expect("get orcs table");
        register_llm_functions(&lua, &orcs).expect("register llm functions");

        let opts = lua.create_table().expect("create opts");
        opts.set("new_session", true).expect("set new_session");

        let result =
            call_llm_handler(&lua, "start".into(), Some(opts)).expect("call with new_session");
        let content: String = result.get("content").expect("get content");
        assert!(
            content.contains("--session-id"),
            "should pass --session-id flag, got: {}",
            content
        );
        let sid: String = result.get("session_id").expect("get session_id");
        assert!(
            sid.contains("test-uuid-1234"),
            "session_id should come from uuidgen, got: {}",
            sid
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

        // Fake orcs.log
        let log_fn = lua
            .create_function(|_, (_level, _msg): (String, String)| Ok(()))
            .expect("create log fn");
        orcs.set("log", log_fn).expect("set log");

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

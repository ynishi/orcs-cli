-- lua_sandbox.lua
-- Sandboxed Lua execution component for LLM tool_call integration.
--
-- Provides a "lua_eval" IntentDef so that the LLM can execute Lua code
-- in a restricted environment via the ActionIntent/Resolve pipeline.
--
-- Execution is delegated to orcs.sandbox_eval(code) — a Rust-backed function
-- that uses mlua's native API for compilation, environment isolation, and
-- instruction-count limiting. This avoids dependency on Lua's `load` and
-- `debug` globals (which are removed by sandbox_lua_globals).
--
-- Security model (enforced by Rust sandbox_eval):
--   - Whitelist-only environment (no os, io, debug, require, load, loadfile, dofile)
--   - Instruction count limit (1M) to prevent infinite loops
--   - Read-only orcs APIs (json, git_info, pwd, read, grep, glob)
--   - Stateless: fresh environment per invocation
--
-- Operations:
--   execute  -> { code: string } -> sandboxed eval result
--   status   -> component health check
--
-- IntentDef registered at init:
--   name = "lua_eval"
--   resolver = Component { builtin::lua_sandbox, "execute" }

-- === Constants ===

local COMPONENT_ID = "lua_sandbox"

-- === Module State ===

local component_settings = {}

-- === Request Handlers ===

--- Handle "execute" operation: run code in sandbox via Rust orcs.sandbox_eval.
--- Payload: { code = "..." }
local function handle_execute(payload)
    if not payload or type(payload.code) ~= "string" then
        return { success = false, error = "payload.code (string) is required" }
    end

    orcs.log("debug", COMPONENT_ID .. ": executing sandboxed code (" .. #payload.code .. " bytes)")

    -- Delegate to Rust-backed sandbox_eval
    local result = orcs.sandbox_eval(payload.code)

    if result.ok then
        -- Build human-readable output for LLM
        local parts = {}
        if result.output and result.output ~= "" then
            parts[#parts + 1] = result.output
        end
        if result.result then
            parts[#parts + 1] = "Return value: " .. result.result
        end
        if #parts == 0 then
            parts[#parts + 1] = "(no output)"
        end

        return {
            success = true,
            data = table.concat(parts, "\n"),
        }
    else
        local parts = {}
        parts[#parts + 1] = "Error: " .. (result.error or "unknown")
        if result.output and result.output ~= "" then
            parts[#parts + 1] = "Output before error:\n" .. result.output
        end

        return {
            success = false,
            error = table.concat(parts, "\n"),
        }
    end
end

--- Handle "status" operation: health check.
local function handle_status(_payload)
    return {
        success = true,
        data = {
            component = COMPONENT_ID,
            stateless = true,
        },
    }
end

-- === Handler Dispatch ===

local handlers = {
    execute = handle_execute,
    status  = handle_status,
}

-- === Component Definition ===

return {
    id = COMPONENT_ID,
    namespace = "builtin",
    subscriptions = {},
    elevated = false,

    init = function(cfg)
        if cfg and type(cfg) == "table" then
            component_settings = cfg
        end

        -- Register "lua_eval" IntentDef so LLM can call it via tool_use
        local reg = orcs.register_intent({
            name = "lua_eval",
            description = "Execute Lua code in a sandboxed environment. "
                .. "Use for data processing, calculations, string manipulation, JSON transformation, "
                .. "or any computational task. The sandbox provides: print(), math, string, table libraries, "
                .. "and read-only orcs APIs (orcs.read, orcs.grep, orcs.glob, orcs.json_encode, orcs.json_decode, "
                .. "orcs.git_info, orcs.pwd). No file writes, no shell access, no network. "
                .. "Code runs statelessly — each call starts fresh. 1M instruction limit.",
            component = "builtin::lua_sandbox",
            operation = "execute",
            timeout_ms = 30000,
            params = {
                code = {
                    type = "string",
                    description = "Lua source code to execute. Use print() for output. Return values are captured.",
                    required = true,
                },
            },
        })

        if reg and reg.ok then
            orcs.log("info", COMPONENT_ID .. ": initialized, lua_eval intent registered")
        else
            orcs.log("warn", COMPONENT_ID .. ": lua_eval intent registration failed: "
                .. ((reg and reg.error) or "unknown"))
        end
    end,

    shutdown = function()
        orcs.log("info", COMPONENT_ID .. " shutdown")
    end,

    on_request = function(request)
        local handler = handlers[request.operation]
        if handler then
            return handler(request.payload)
        end
        return { success = false, error = "unknown operation: " .. tostring(request.operation) }
    end,

    on_signal = function(signal)
        if signal.kind == "Veto" then
            return "Abort"
        end
        return "Handled"
    end,
}

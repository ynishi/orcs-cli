-- delegate_worker.lua
-- Default worker agent: handles DelegateTask events for sub-agent delegation.
--
-- Spawned by agent_mgr when an LLM-initiated delegate_task action is dispatched.
-- Runs an independent LLM session to complete a delegated task, then emits
-- a DelegateResult event for context injection into the next concierge turn.
--
-- Operations:
--   process  — execute delegated task (from DelegateTask event)
--   status   — return current state for observability
--
-- This is a spawned delegate agent (child of agent_mgr).
-- It runs in its own Lua VM with an independent event loop.

-- === Module State ===

local busy = false
local task_count = 0
local last_request_id = nil
local last_cost = nil

-- === Request Handlers ===

--- Handle status: return current agent state for observability.
local function handle_status()
    return {
        success = true,
        data = {
            busy = busy,
            task_count = task_count,
            last_request_id = last_request_id,
            last_cost = last_cost,
        },
    }
end

--- Handle process: execute a delegated task.
--- Uses pcall to guarantee busy=false even if orcs.llm()/orcs.request() throws.
local function handle_process(payload)
    local request_id = payload.request_id or "unknown"
    local description = payload.description or ""
    local context = payload.context or ""
    local llm_config = payload.llm_config or {}
    local delegate_backend = payload.delegate_backend  -- FQN of backend component (nil = use orcs.llm)

    busy = true
    task_count = task_count + 1
    last_request_id = request_id

    orcs.log("info", "delegate-worker: starting task " .. request_id
        .. (delegate_backend and (" via " .. delegate_backend) or " via orcs.llm"))
    orcs.output("[Delegate:" .. request_id .. "] Starting task...")

    local ok, pcall_err = pcall(function()
        -- Build task-focused prompt
        local prompt_parts = {}
        prompt_parts[#prompt_parts + 1] = "You are a specialized sub-agent handling a delegated task."
        prompt_parts[#prompt_parts + 1] = "Complete the following task thoroughly and report your findings concisely."
        prompt_parts[#prompt_parts + 1] = "You have access to file tools (read, write, grep, glob, exec) to assist."
        if context ~= "" then
            prompt_parts[#prompt_parts + 1] = "## Context\n" .. context
        end
        prompt_parts[#prompt_parts + 1] = "## Task\n" .. description
        local prompt = table.concat(prompt_parts, "\n\n")

        local summary, cost, sess_id, err

        if delegate_backend then
            -- Route through external backend component (e.g. custom::my_llm)
            local result = orcs.request(delegate_backend, "process", {
                message = prompt,
            }, { timeout_ms = 600000 })
            if result and result.success then
                summary = (result.data and result.data.response) or ""
                cost = result.data and result.data.cost
                sess_id = result.data and result.data.session_id
            else
                err = (result and result.error) or "backend request failed"
            end
        else
            -- Use built-in orcs.llm() with configured provider
            local opts = {}
            if llm_config.provider    then opts.provider    = llm_config.provider end
            if llm_config.model       then opts.model       = llm_config.model end
            if llm_config.base_url    then opts.base_url    = llm_config.base_url end
            if llm_config.api_key     then opts.api_key     = llm_config.api_key end
            if llm_config.temperature then opts.temperature = llm_config.temperature end
            if llm_config.max_tokens  then opts.max_tokens  = llm_config.max_tokens end
            if llm_config.timeout     then opts.timeout     = llm_config.timeout end
            opts.resolve = true  -- Enable tool-use for the delegate
            opts.hil_intents = true  -- Propagate Suspended for HIL approval

            local resp = orcs.llm(prompt, opts)
            if resp and resp.ok then
                summary = resp.content or ""
                cost = resp.cost
                sess_id = resp.session_id
            else
                err = (resp and resp.error) or "unknown error"
            end
        end

        last_cost = cost

        if not err then
            orcs.output("[Delegate:" .. request_id .. "] " .. (summary or ""))
            orcs.emit_event("DelegateResult", "completed", {
                request_id = request_id,
                summary = summary or "",
                success = true,
                cost = cost,
                session_id = sess_id,
            })
            orcs.log("info", string.format(
                "delegate-worker: task %s completed (cost=%.4f)",
                request_id, cost or 0
            ))
        else
            orcs.output_with_level("[Delegate:" .. request_id .. "] Error: " .. tostring(err), "error")
            orcs.emit_event("DelegateResult", "completed", {
                request_id = request_id,
                error = err,
                success = false,
            })
            orcs.log("error", "delegate-worker: task " .. request_id .. " failed: " .. err)
        end
    end)

    -- Guarantee busy reset regardless of pcall outcome
    busy = false

    if not ok then
        -- If Suspended for HIL approval, re-throw after cleanup.
        -- pcall was only needed to guarantee busy=false; the ChannelRunner
        -- needs the original ComponentError::Suspended (preserved as mlua
        -- WrappedFailure userdata) to trigger the HIL approval flow.
        -- Type-safe check via Rust downcast (no fragile string matching).
        local suspended = orcs.suspended_info(pcall_err)
        if suspended then
            orcs.log("info", "delegate-worker: task " .. request_id
                .. " suspended for HIL approval: " .. suspended.description)
            error(pcall_err)
        end

        local err_str = tostring(pcall_err)
        orcs.output_with_level("[Delegate:" .. request_id .. "] Error: " .. err_str, "error")
        orcs.emit_event("DelegateResult", "completed", {
            request_id = request_id,
            error = err_str,
            success = false,
        })
        orcs.log("error", "delegate-worker: task " .. request_id .. " threw: " .. err_str)
    end

    return { success = true }
end

-- === Component Definition ===

return {
    id = "delegate-worker",
    namespace = "builtin",
    subscriptions = {"DelegateTask"},

    on_request = function(request)
        local operation = request.operation or ""

        if operation == "status" then
            return handle_status()
        end

        if operation == "process" then
            return handle_process(request.payload or {})
        end

        return { success = false, error = "unknown operation: " .. tostring(operation) }
    end,

    on_signal = function(signal)
        if signal.kind == "Veto" then
            return "Abort"
        end
        return "Handled"
    end,
}

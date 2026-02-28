-- delegate_worker.lua
-- Worker agent: handles delegated tasks for sub-agent delegation.
--
-- Two modes of operation:
--
-- 1. **Shared worker** (legacy): spawned once at init, subscribes to "DelegateTask"
--    events and processes them sequentially.
--
-- 2. **Per-delegation worker** (parallel): spawned per delegation with
--    `_delegate_payload` global injected via `spawn_runner({ globals = ... })`.
--    Subscribes to a unique "DelegateTask-{request_id}" event for isolation.
--    Multiple per-delegation workers run concurrently in independent Lua VMs.
--
-- Operations:
--   process  — execute delegated task (from DelegateTask/DelegateTask-{id} event)
--   status   — return current state for observability
--
-- This is a spawned delegate agent (child of agent_mgr).
-- It runs in its own Lua VM with an independent event loop.

-- === Per-delegation payload (injected via globals, nil for shared worker) ===

local delegate_payload = _delegate_payload  -- luacheck: ignore 113

-- Configurable timeouts (overridden via _delegate_timeout_ms global if set)
local delegate_timeout_ms = (_delegate_timeout_ms and type(_delegate_timeout_ms) == "number")
    and _delegate_timeout_ms
    or 600000  -- 10 minutes default

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
            mode = delegate_payload and "per-delegation" or "shared",
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
            }, { timeout_ms = delegate_timeout_ms })
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
            if llm_config.provider       then opts.provider       = llm_config.provider end
            if llm_config.model          then opts.model           = llm_config.model end
            if llm_config.base_url       then opts.base_url        = llm_config.base_url end
            if llm_config.api_key        then opts.api_key         = llm_config.api_key end
            if llm_config.temperature    then opts.temperature     = llm_config.temperature end
            if llm_config.max_tokens     then opts.max_tokens      = llm_config.max_tokens end
            opts.timeout = llm_config.timeout or math.floor(delegate_timeout_ms / 1000)
            opts.overall_timeout = math.floor(delegate_timeout_ms / 1000)
            if llm_config.max_tool_turns then opts.max_tool_turns  = llm_config.max_tool_turns end
            opts.resolve = true  -- Enable tool-use for the delegate
            opts.hil_intents = true  -- Propagate Suspended for HIL approval

            local resp = orcs.llm(prompt, opts)

            -- Dynamic budget escalation: if tool_loop_limit hit, resume same
            -- session with additional turns (+10, single extension only).
            if resp and resp.error_kind == "tool_loop_limit" and resp.session_id then
                local initial_turns = opts.max_tool_turns or 10
                local extension = math.min(initial_turns, 10)
                orcs.log("warn", string.format(
                    "delegate-worker: task %s hit tool_loop_limit at %d turns, "
                    .. "extending by %d (session=%s)",
                    request_id, initial_turns, extension,
                    (resp.session_id or ""):sub(1, 12)
                ))
                orcs.output(string.format(
                    "[WARN] [Delegate:%s] Work incomplete: tool turn limit reached. "
                    .. "Extending by %d turns...",
                    request_id, extension
                ))
                opts.session_id = resp.session_id
                opts.max_tool_turns = extension
                resp = orcs.llm(
                    "You ran out of tool turns. Continue and complete the task. "
                    .. "Prioritize the most important remaining work.",
                    opts
                )
            end

            if resp and resp.ok then
                summary = resp.content or ""
                cost = resp.cost
                sess_id = resp.session_id
                if resp.stop_reason == "max_tokens" then
                    orcs.log("warn", string.format(
                        "delegate-worker: task %s response truncated by max_tokens (content_len=%d)",
                        request_id, #summary
                    ))
                    orcs.output_with_level(
                        "[Delegate:" .. request_id .. "] Warning: response truncated by output token limit.",
                        "warn"
                    )
                end
            else
                err = (resp and resp.error) or "unknown error"
            end
        end

        last_cost = cost

        if not err then
            local display = (summary and summary ~= "") and summary or "(completed)"
            orcs.output("[Delegate:" .. request_id .. "] " .. display)
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

    -- Per-delegation mode: self-terminate after task completion.
    -- The runner is no longer needed (unique subscription won't receive more events).
    if delegate_payload and orcs.request_stop then
        orcs.log("info", "delegate-worker: " .. request_id .. " requesting self-stop")
        orcs.request_stop()
    end

    return { success = true }
end

-- === Component Definition ===

-- Per-delegation mode: unique ID and targeted subscription for isolation.
-- Shared mode: generic ID and broad DelegateTask subscription (legacy).
local component_id = delegate_payload
    and ("delegate-" .. (delegate_payload.request_id or "adhoc"))
    or "delegate-worker"

local subscriptions = delegate_payload
    and { "DelegateTask-" .. delegate_payload.request_id }
    or { "DelegateTask" }

return {
    id = component_id,
    namespace = "builtin",
    subscriptions = subscriptions,

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

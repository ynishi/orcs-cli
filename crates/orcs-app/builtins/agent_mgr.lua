-- agent_mgr.lua
-- Agent Manager component: routes UserInput to specialized workers.
--
-- Architecture (Event-driven + RPC hybrid, parent-child agent hierarchy):
--   agent_mgr (Router Component, subscriptions=UserInput,DelegateResult)
--     │
--     ├─ @prefix commands → targeted RPC to specific components
--     │
--     ├─ LLM tasks → emit "AgentTask" event (fire-and-forget, non-blocking)
--     │       │
--     │       ▼ (broadcast via EventBus)
--     │   concierge (spawned via orcs.spawn_runner({ builtin = "concierge.lua" }))
--     │     — subscriptions=AgentTask
--     │     — calls orcs.llm(resolve=true) for tool-use auto-dispatch
--     │     — registers "delegate_task" IntentDef for LLM-initiated delegation
--     │     — calls orcs.output() directly to display response (output_to_io inherited)
--     │     — agent_mgr is NOT blocked during LLM processing
--     │     — supports status RPC: busy, session_id, turn_count, last_cost, provider, model
--     │
--     ├─ Delegation → LLM tool_call("delegate_task") → agent_mgr.delegate()
--     │       │          → emit "DelegateTask" event (fire-and-forget)
--     │       ▼
--     │   delegate-worker (spawned via orcs.spawn_runner({ builtin = "delegate_worker.lua" }))
--     │     — subscriptions=DelegateTask
--     │     — runs independent orcs.llm(resolve=true) session for the delegated task
--     │     — calls orcs.output() directly to display results
--     │     — emits "DelegateResult" event on completion
--     │     — agent_mgr stores results for context injection in next turn
--     │     — supports status RPC: busy, task_count, last_request_id, last_cost
--     │
--     └─ init() → targeted RPC (ping) to concierge for health check
--
-- Concierge mode:
--   When concierge is set (e.g. concierge="custom::my_llm"), agent_mgr does NOT spawn
--   the builtin concierge. Instead, the external component (loaded via config.toml)
--   subscribes to "AgentTask" events and handles LLM processing independently.
--   delegate-worker IS always spawned (delegation is orthogonal to LLM backend).
--
-- Tool execution:
--   LLM uses tool_use (function calling) to invoke:
--     - 8 builtin Internal tools (read, write, grep, glob, mkdir, remove, mv, exec)
--     - Dynamically registered skill IntentDefs (from skill_manager.recommend)
--     - delegate_task: delegates work to an independent sub-agent
--   All tool calls are auto-dispatched via IntentRegistry resolve loop.
--
-- User @command routing (direct user input, not LLM-generated):
--   @shell <cmd>           → builtin::shell (passthrough)
--   @tool <subcmd> ...     → builtin::tool (passthrough)
--   @skill <op> ...        → skill::skill_manager (operation dispatch)
--   @profile <op> ...      → profile::profile_manager (passthrough via handle_input)
--   @foundation <op> ...   → foundation::foundation_manager (operation dispatch)
--
-- Foundation segments (from agent.md via foundation_manager):
--   system — always at top (identity/core rules)
--   task   — before history (project context)
--   guard  — before user message (output constraints)
--
-- Console metrics (from console_metrics):
--   git branch, working directory, timestamp
--
-- Prompt placement strategies (configurable via [components.settings.agent_mgr]):
--   top    — [f:system][Metrics][Tools][f:task] [History][Delegation][f:guard][UserInput]
--   bottom — [f:task][History] [f:system][Metrics][Tools][Delegation][f:guard][UserInput]
--   both   — [f:system][Metrics][Tools][f:task] [History(100K)] [f:system][Metrics][Tools][f:task] [Delegation][f:guard][UserInput]  (default)
--
-- "both" literally places the full system context (f:system, Metrics, Skills/Tools)
-- at BOTH top and bottom, sandwiching the long history. After ~100K tokens of history,
-- the bottom anchor re-grounds the model on its identity, active agents, and tools.
-- Metrics (active agents, provider state) are especially important to repeat.
-- TODO: "auto" mode planned — auto(both_cond=N) switches "both"/"top" by context size.
--
-- ## Config: [components.settings.agent_mgr]
--
-- Settings are received via init(cfg) from config.toml.
-- The llm_* keys are mapped to orcs.llm() opts via llm_key_map (llm_provider → provider, etc.).
--
-- | Key               | Type                              | Default          | Description                        |
-- |-------------------|-----------------------------------|------------------|------------------------------------|
-- | concierge         | string (FQN)                      | nil              | Main conversation LLM backend      |
-- | delegate_backend  | string (FQN)                      | nil              | Delegate-worker LLM backend        |
-- | prompt_placement  | "top" | "both" | "bottom"         | "both"           | Where skills/tools appear in prompt|
-- | llm_provider      | "ollama" | "openai" | "anthropic" | "ollama"         | LLM provider (non-concierge only)  |
-- | llm_model         | string                            | provider default | Model name (non-concierge only)    |
-- | llm_base_url      | string                            | provider default | Provider base URL                  |
-- | llm_api_key       | string                            | env var fallback | API key                            |
-- | llm_temperature   | number                            | (none)           | Sampling temperature               |
-- | llm_max_tokens    | number                            | (none)           | Max completion tokens              |
-- | llm_timeout       | number                            | 120              | Request timeout (seconds)          |
--
-- ## Concierge Mode (concierge setting)
--
-- When `concierge` is set to a component FQN (e.g. "custom::my_llm"), agent_mgr
-- replaces the builtin concierge with the specified external component:
--
--   1. Builtin concierge is NOT spawned (the external component handles AgentTask events)
--   2. llm_* settings are UNUSED for main conversation (the external component
--      manages its own LLM configuration independently)
--
-- The concierge component must:
--   - Subscribe to "AgentTask" events (subscriptions = {"AgentTask"})
--   - Handle on_request(operation="process") with payload.message
--   - Return { success=true, data={ response="..." } }
--   - Call orcs.output() to display the response to the user
--
-- ## Delegate Backend (delegate_backend setting)
--
-- Controls which LLM backend the delegate-worker uses (independent from concierge).
-- When set to a component FQN, delegate-worker routes via RPC:
--   orcs.request(delegate_backend, "process", { message = prompt })
-- When unset (nil), delegate-worker uses orcs.llm() with llm_* settings.
--
-- The backend component must handle on_request(operation="process") same as concierge.
--
-- ### Config Examples
--
-- Built-in mode (default — uses builtin concierge + orcs.llm for both):
--   [components.settings.agent_mgr]
--   llm_provider = "ollama"
--   llm_model    = "qwen2.5-coder:7b"
--
-- Concierge + delegate both via external backend:
--   [components]
--   load = ["agent_mgr", "...", "my_llm"]   # load the external backend component
--
--   [components.settings.agent_mgr]
--   concierge        = "custom::my_llm"   # main conversation → external backend
--   delegate_backend = "custom::my_llm"   # delegate-worker   → external backend
--
-- Mixed: concierge via external backend, delegate via built-in ollama:
--   [components.settings.agent_mgr]
--   concierge    = "custom::my_llm"
--   # delegate_backend omitted → delegate-worker uses orcs.llm() with llm_* below
--   llm_provider = "ollama"
--   llm_model    = "qwen2.5-coder:7b"
--
-- NOTE: Config layering — project-local .orcs/config.toml replaces global settings
-- per component name (not per key). If the project config defines
-- [components.settings.agent_mgr], it must include ALL needed keys (including
-- concierge, delegate_backend) because the global agent_mgr settings are replaced.
--
-- ## Global Config: cfg._global (injected by builder)
--
-- All components receive global config under cfg._global:
--   cfg._global.debug            (bool)    — debug mode
--   cfg._global.model.default    (string)  — default model name
--   cfg._global.model.temperature(number)  — default temperature
--   cfg._global.model.max_tokens (number?) — default max tokens
--   cfg._global.hil.auto_approve (bool)    — auto-approve requests
--   cfg._global.hil.timeout_ms   (number)  — approval timeout (ms)
--   cfg._global.ui.verbose       (bool)    — verbose output
--   cfg._global.ui.color         (bool)    — color output
--   cfg._global.ui.emoji         (bool)    — emoji output

-- === Constants & Settings ===

local HISTORY_LIMIT = 10  -- Max recent conversation entries to include as context

-- Component settings (populated from config in init())
local component_settings = {}

-- FQN of the concierge agent (set in init()).
-- Either the spawned builtin or the external FQN from config.
-- Used by init() for health check RPC and dispatch_llm() guard check.
local concierge_fqn = nil

-- FQN of the spawned delegate worker Component (set in init()).
local delegate_worker_fqn = nil

-- Completed delegation results for context injection into next LLM turn.
-- Stored as array of {request_id, summary, success, cost, timestamp}.
-- Oldest entries are evicted when limit is exceeded.
local delegation_results = {}
local DELEGATION_RESULTS_LIMIT = 5

-- Counter for generating unique delegation request IDs.
local delegation_counter = 0

-- Valid prompt placement strategies
local VALID_PLACEMENTS = { top = true, both = true, bottom = true }

-- Mapping from component_settings keys (llm_*) to LLM config keys (without prefix).
-- Single source of truth: used by dispatch_llm(), delegate operation, and init() ping.
local LLM_KEY_MAP = {
    llm_provider    = "provider",
    llm_model       = "model",
    llm_base_url    = "base_url",
    llm_api_key     = "api_key",
    llm_temperature = "temperature",
    llm_max_tokens  = "max_tokens",
    llm_timeout     = "timeout",
}

--- Extract LLM config from component_settings using LLM_KEY_MAP.
--- @param keys table|nil  Optional subset of LLM_KEY_MAP keys to extract.
---                        If nil, all keys are extracted.
--- @return table  LLM config table with mapped keys.
local function extract_llm_config(keys)
    local config = {}
    local map = keys or LLM_KEY_MAP
    for src_key, dst_key in pairs(map) do
        if component_settings[src_key] ~= nil then
            config[dst_key] = component_settings[src_key]
        end
    end
    return config
end

-- === Routing ===

--- Route table: prefix → { target, dispatch }
--- dispatch = "passthrough": send full body as message with operation="input"
---                           (component parses the message internally)
--- dispatch = "operation":   parse first word of body as operation, rest as args
local routes = {
    shell      = { target = "builtin::shell",                 dispatch = "passthrough" },
    skill      = { target = "skill::skill_manager",           dispatch = "operation" },
    profile    = { target = "profile::profile_manager",       dispatch = "passthrough" },
    tool       = { target = "builtin::tool",                  dispatch = "passthrough" },
    foundation = { target = "foundation::foundation_manager", dispatch = "operation" },
}

--- Parse @prefix from message. Returns (prefix, rest) or (nil, original).
local function parse_route(message)
    local prefix, rest = message:match("^@(%w+)%s+(.+)$")
    if prefix then
        return prefix:lower(), rest
    end
    -- Also match bare @prefix without args
    local bare = message:match("^@(%w+)%s*$")
    if bare then
        return bare:lower(), ""
    end
    return nil, message
end

--- Execute a routed @command via RPC to the target component.
local function dispatch_route(route, body)
    local operation, payload
    if route.dispatch == "passthrough" then
        operation = "input"
        payload = { message = body }
    else
        local op, args = body:match("^(%S+)%s+(.+)$")
        if op then
            operation = op:lower()
            payload = { message = args, name = args }
        else
            operation = body ~= "" and body:lower() or "status"
            payload = {}
        end
    end

    local resp = orcs.request(route.target, operation, payload)
    if resp and resp.success then
        -- Operation-dispatch targets don't call orcs.output; display result here
        if route.dispatch ~= "passthrough" then
            local data = resp.data
            if type(data) == "table" then
                data = orcs.json_encode(data)
            end
            orcs.output(tostring(data or "ok"))
        end
        orcs.emit_event("Extension", "route_response", {
            target = route.target,
            operation = operation,
            message = body,
        })
        return {
            success = true,
            data = {
                response = resp.data,
                source = route.target,
            },
        }
    else
        local err = (resp and resp.error) or "request failed"
        orcs.output("[AgentMgr] @" .. route.target .. " error: " .. err, "error")
        return { success = false, error = err }
    end
end

--- Truncate a string to at most max_bytes bytes without splitting multi-byte
--- UTF-8 characters.  Plain string.sub() can cut in the middle of a
--- multi-byte sequence, producing invalid UTF-8 that Rust's String rejects.
local function utf8_truncate(s, max_bytes)
    if #s <= max_bytes then return s end
    local pos = max_bytes
    -- Walk backwards past any continuation bytes (10xxxxxx)
    while pos > 0 do
        local b = string.byte(s, pos)
        if b < 0x80 or b >= 0xC0 then break end
        pos = pos - 1
    end
    -- pos is at an ASCII byte or a start byte; verify the full character fits
    if pos > 0 then
        local b = string.byte(s, pos)
        local char_len = 1
        if     b >= 0xF0 then char_len = 4
        elseif b >= 0xE0 then char_len = 3
        elseif b >= 0xC0 then char_len = 2
        end
        if pos + char_len - 1 > max_bytes then
            return s:sub(1, pos - 1)
        end
        return s:sub(1, pos + char_len - 1)
    end
    return ""
end

--- Fetch recent conversation history from EventBoard.
--- Called in parent (agent_mgr) because board_recent is an emitter function
--- unavailable to child workers.
local function fetch_history_context()
    if not orcs.board_recent then
        return ""
    end
    local ok, entries = pcall(orcs.board_recent, HISTORY_LIMIT)
    orcs.log("debug", "board_recent returned " .. (ok and tostring(#(entries or {})) or "error") .. " entries")
    if not ok or not entries or #entries == 0 then
        return ""
    end
    local lines = {}
    for _, entry in ipairs(entries) do
        local payload = entry.payload or {}
        local text = payload.message or payload.content or payload.response
        if text and type(text) == "string" and text ~= "" then
            -- source is a ComponentId table { namespace, name }, not a string
            local raw_src = entry.source
            local src
            if type(raw_src) == "table" then
                src = raw_src.name or raw_src.namespace or "unknown"
            else
                src = tostring(raw_src or "unknown")
            end
            lines[#lines + 1] = string.format("- [%s] %s", src, utf8_truncate(text, 200))
        end
    end
    if #lines > 0 then
        return table.concat(lines, "\n")
    end
    return ""
end

--- Fetch completed delegation results for context injection.
--- Returns a formatted string summarizing recent delegation outcomes,
--- or empty string if no results are available.
local function fetch_delegation_context()
    if #delegation_results == 0 then
        return ""
    end

    local lines = {}
    for _, r in ipairs(delegation_results) do
        local status = r.success and "completed" or "failed"
        local summary = utf8_truncate(r.summary or "", 500)
        lines[#lines + 1] = string.format("- [%s] (%s) %s", r.request_id, status, summary)
    end

    return "## Delegation Results\n"
        .. "The following tasks were delegated to sub-agents and have completed:\n"
        .. table.concat(lines, "\n")
end

--- Default route: emit AgentTask event for LLM processing.
--- The event is broadcast to the concierge agent (builtin or external).
--- agent_mgr returns immediately — the concierge handles LLM processing and output
--- asynchronously via its own event loop.
local function dispatch_llm(message)
    -- Guard: concierge agent (builtin or external) must be available.
    -- concierge_fqn is set in init() for both builtin spawn and external config.
    if not concierge_fqn then
        orcs.output_with_level("[AgentMgr] Error: no LLM backend available (concierge not initialized)", "error")
        return { success = false, error = "no LLM backend available" }
    end

    -- Gather history in parent context (board_recent unavailable to child workers)
    local history_context = fetch_history_context()

    -- Gather completed delegation results for context injection
    local delegation_context = fetch_delegation_context()

    -- Resolve prompt placement strategy from config
    local placement = component_settings.prompt_placement or "both"
    if not VALID_PLACEMENTS[placement] then
        orcs.log("warn", "Invalid prompt_placement '" .. placement .. "', falling back to both")
        placement = "both"
    end

    -- Extract LLM-specific config (llm_* keys → config table without prefix)
    local llm_config = extract_llm_config()

    -- Emit AgentTask event (fire-and-forget, non-blocking).
    -- The concierge agent receives this and processes asynchronously.
    local payload = {
        message = message,
        prompt_placement = placement,
        llm_config = llm_config,
        history_context = history_context,
        delegation_context = delegation_context,
    }
    local delivered = orcs.emit_event("AgentTask", "process", payload)

    -- emit_event returns true if at least one channel received the event.
    -- NOTE: The count includes the emitter's own channel (agent_mgr), so
    -- delivered=true does not guarantee a real subscriber matched. However,
    -- delivered=false reliably detects that NO channel was reachable (e.g.
    -- all workers crashed and their channels are closed).
    if not delivered then
        orcs.output_with_level("[AgentMgr] Error: AgentTask event was not delivered to any channel", "error")
        return { success = false, error = "AgentTask event delivery failed" }
    end

    orcs.log("debug", string.format(
        "dispatch_llm: emitted AgentTask event (message=%d chars)",
        #message
    ))

    -- Return immediately — worker handles output via orcs.output()
    return {
        success = true,
        data = {
            dispatched = true,
            message = message,
            source = "event:AgentTask",
        },
    }
end

-- === Component Definition ===

return {
    id = "agent_mgr",
    namespace = "builtin",
    subscriptions = {"UserInput", "DelegateResult"},
    output_to_io = true,
    elevated = true,
    child_spawner = true,

    on_request = function(request)
        local operation = request.operation or "input"

        -- Handle DelegateResult events from delegate-worker.
        -- Store completed results for context injection into next LLM turn.
        if operation == "completed" then
            local payload = request.payload or {}
            local request_id = payload.request_id
            if request_id then
                delegation_results[#delegation_results + 1] = {
                    request_id = request_id,
                    summary = payload.summary or payload.error or "",
                    success = payload.success or false,
                    cost = payload.cost,
                    timestamp = os.time(),
                }
                -- Evict oldest entries
                while #delegation_results > DELEGATION_RESULTS_LIMIT do
                    table.remove(delegation_results, 1)
                end
                orcs.log("info", string.format(
                    "AgentMgr: delegation %s %s (stored, %d results buffered)",
                    request_id,
                    payload.success and "completed" or "failed",
                    #delegation_results
                ))
            end
            return { success = true }
        end

        -- Handle list_active: return agent summary for console_metrics.
        -- Returns locally cached info only (no cross-agent RPC).
        --
        -- Safety: This handler reads only local variables (concierge_fqn,
        -- delegate_worker_fqn, component_settings, delegation_counter,
        -- delegation_results). No blocking RPC calls are made, so it
        -- cannot contribute to deadlock regardless of ChannelRunner state.
        --
        -- Deadlock scenario avoided: if this handler queried child agents
        -- (e.g., orcs.request(concierge_fqn, "status")), the call chain
        -- would be: console_metrics → agent_mgr → concierge. If the
        -- concierge's ChannelRunner is blocked on an LLM call, agent_mgr's
        -- on_request would also block, starving all other RPCs to agent_mgr.
        if operation == "list_active" then
            local agents = {}

            -- Concierge info (FQN only, no live status query)
            agents.concierge_fqn = concierge_fqn

            -- Delegate-worker info (FQN only, no live status query)
            agents.delegate_fqn = delegate_worker_fqn

            -- LLM config (for display when using builtin concierge)
            local concierge_cfg = component_settings.concierge
            if concierge_cfg == "" then concierge_cfg = nil end
            if not concierge_cfg then
                agents.llm_provider = component_settings.llm_provider
                agents.llm_model = component_settings.llm_model
            end

            -- Delegation stats
            agents.delegation_count = delegation_counter
            agents.delegation_completed = #delegation_results

            return { success = true, data = agents }
        end

        -- Handle delegation requests from concierge (via IntentRegistry dispatch).
        -- Spawns delegate-worker task via DelegateTask event.
        if operation == "delegate" then
            local payload = request.payload or {}
            local description = payload.description or ""
            if description == "" then
                return { success = false, error = "delegate_task requires a description" }
            end

            -- Guard: delegate-worker must be available
            if not delegate_worker_fqn then
                orcs.output_with_level("[AgentMgr] Error: delegate-worker not available", "error")
                return { success = false, error = "delegate-worker not available" }
            end

            -- Generate unique request ID
            delegation_counter = delegation_counter + 1
            local request_id = string.format("d%03d", delegation_counter)

            -- Extract LLM config for the delegate
            local llm_config = extract_llm_config()

            -- Resolve delegate_backend FQN (independent from concierge)
            local delegate_backend = component_settings.delegate_backend
            if delegate_backend == "" then delegate_backend = nil end

            -- Emit DelegateTask event (fire-and-forget)
            local delivered = orcs.emit_event("DelegateTask", "process", {
                request_id = request_id,
                description = description,
                context = payload.context or "",
                llm_config = llm_config,
                delegate_backend = delegate_backend,
            })

            if not delivered then
                orcs.output_with_level("[AgentMgr] Error: DelegateTask event not delivered", "error")
                return { success = false, error = "DelegateTask event delivery failed" }
            end

            orcs.log("info", string.format(
                "AgentMgr: delegated task %s (%d chars)",
                request_id, #description
            ))

            return {
                success = true,
                data = {
                    request_id = request_id,
                    message = "Task delegated to sub-agent (id: " .. request_id .. "). Results will appear in your context on the next turn.",
                },
            }
        end

        -- Handle UserInput events (default path).
        local message = request.payload
        if type(message) == "table" then
            message = message.message or message.content or ""
        end
        if type(message) ~= "string" or message == "" then
            return { success = false, error = "empty message" }
        end

        orcs.log("info", "AgentMgr received: " .. utf8_truncate(message, 50))

        -- Parse @prefix routing
        local prefix, body = parse_route(message)

        if prefix then
            local route = routes[prefix]
            if route then
                orcs.log("info", "AgentMgr routing @" .. prefix .. " -> " .. route.target)
                return dispatch_route(route, body)
            else
                -- Unknown @prefix: pass full message to concierge as-is
                orcs.log("debug", "AgentMgr: unknown prefix @" .. prefix .. ", falling through to llm")
            end
        end

        -- Default: route to concierge agent
        return dispatch_llm(message)
    end,

    on_signal = function(signal)
        if signal.kind == "Veto" then
            return "Abort"
        end
        return "Ignored"
    end,

    init = function(cfg)
        -- Store component settings from [components.settings.agent_mgr]
        if cfg and type(cfg) == "table" then
            if next(component_settings) ~= nil then
                orcs.log("warn", "agent_mgr: re-initialization detected, overwriting previous settings")
            end
            component_settings = cfg
            orcs.log("debug", "agent_mgr: config received: " .. orcs.json_encode(cfg))
        end

        -- Normalize concierge: Lua treats "" as truthy, so coerce to nil
        local concierge = component_settings.concierge
        if concierge == "" then concierge = nil end

        local placement = component_settings.prompt_placement or "both"
        local llm_provider = component_settings.llm_provider or "(default)"
        local llm_model = component_settings.llm_model or "(default)"
        orcs.log("info", string.format(
            "agent_mgr initializing (concierge=%s, prompt_placement=%s, llm=%s/%s)...",
            concierge or "off", placement, llm_provider, llm_model
        ))

        -- Spawn concierge agent (handles AgentTask events for LLM processing).
        -- When concierge is set to an external FQN, skip spawning the builtin.
        if not concierge then
            local result = orcs.spawn_runner({ builtin = "concierge.lua" })
            if result.ok then
                concierge_fqn = result.fqn
                orcs.log("info", "spawned concierge agent (fqn=" .. concierge_fqn .. ")")

                -- Startup health check: verify LLM provider connectivity
                local ping_ok, ping_err = pcall(function()
                    local ping_config = extract_llm_config({
                        llm_provider = "provider",
                        llm_model    = "model",
                        llm_base_url = "base_url",
                        llm_api_key  = "api_key",
                    })

                    local ping_result = orcs.request(concierge_fqn, "ping", {
                        llm_config = ping_config,
                    })
                    if ping_result and ping_result.success and ping_result.data then
                        local d = ping_result.data
                        orcs.log("info", string.format(
                            "LLM provider ready: %s @ %s (status=%s, latency=%dms)",
                            d.provider or "?", d.base_url or "?",
                            tostring(d.status or "?"), d.latency_ms or 0
                        ))
                    else
                        local err = (ping_result and ping_result.data and ping_result.data.error)
                            or (ping_result and ping_result.error)
                            or "unknown"
                        local kind = (ping_result and ping_result.data and ping_result.data.error_kind)
                            or "unknown"
                        orcs.log("warn", string.format(
                            "LLM provider unreachable: %s/%s (%s: %s)",
                            component_settings.llm_provider or "ollama",
                            component_settings.llm_model or "(default)",
                            kind, err
                        ))
                    end
                end)
                if not ping_ok then
                    orcs.log("warn", "LLM health check failed: " .. tostring(ping_err))
                end
            else
                orcs.log("error", "failed to spawn concierge: " .. (result.error or ""))
            end
        else
            concierge_fqn = concierge
            orcs.log("info", "concierge='" .. concierge .. "': using external agent")
        end

        -- Spawn delegate-worker agent (handles DelegateTask events).
        -- Always spawned: delegation is orthogonal to the concierge choice.
        local delegate = orcs.spawn_runner({ builtin = "delegate_worker.lua" })
        if delegate.ok then
            delegate_worker_fqn = delegate.fqn
            orcs.log("info", "spawned delegate-worker agent (fqn=" .. delegate_worker_fqn .. ")")
        else
            orcs.log("warn", "failed to spawn delegate-worker: " .. (delegate.error or ""))
        end

        -- Register observability hook: log routing outcomes
        if orcs.hook then
            local ok, err = pcall(function()
                orcs.hook("builtin::agent_mgr:request.post_dispatch", function(ctx)
                    local result = ctx.result
                    if result and type(result) == "table" then
                        local source = (result.data and result.data.source) or "unknown"
                        local status = result.success and "ok" or "fail"
                        orcs.log("info", string.format(
                            "[hook:routing] source=%s status=%s", source, status
                        ))
                    end
                    return ctx
                end)
            end)
            if ok then
                orcs.log("info", "registered routing observability hook")
            else
                orcs.log("warn", "failed to register hook: " .. tostring(err))
            end
        end

        local backend = concierge_fqn or "failed"
        local delegate_status = delegate_worker_fqn and "ok" or "off"
        orcs.output("[AgentMgr] Ready (backend: " .. backend .. ", delegate: " .. delegate_status .. ")")
        orcs.log("info", "agent_mgr initialized")
    end,

    shutdown = function()
        orcs.log("info", "agent_mgr shutdown")
    end,
}

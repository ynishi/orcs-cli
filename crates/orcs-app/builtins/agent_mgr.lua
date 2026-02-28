-- agent_mgr.lua
-- Agent Manager component: routes UserInput to CommonAgent instances and workers.
--
-- Architecture (Event-driven + RPC hybrid, parent-child agent hierarchy):
--   agent_mgr (Router Component, subscriptions=UserInput,DelegateResult)
--     │
--     ├─ @prefix commands → targeted RPC to specific components or agents
--     │
--     ├─ LLM tasks → emit "AgentTask" event (fire-and-forget, non-blocking)
--     │       │
--     │       ▼ (broadcast via EventBus)
--     │   concierge (spawned via orcs.spawn_runner({ builtin = "concierge.lua" }))
--     │     — subscriptions=AgentTask
--     │     — calls orcs.llm(resolve=true) for tool-use auto-dispatch
--     │     — registers "delegate_task" IntentDef for LLM-initiated delegation
--     │     — calls orcs.output() directly to display response
--     │     — agent_mgr is NOT blocked during LLM processing
--     │     — supports status RPC: busy, session_id, turn_count, last_cost, provider, model
--     │
--     ├─ Config-based agents ([components.settings.agent_mgr.agents.<name>])
--     │     — defined in config.toml as pure data (name, expertise, llm overrides)
--     │     — registered in agent_registry at init (NOT spawned)
--     │     — spawned on-demand when @prefix is used (lazy instantiation)
--     │     — RPC-only (subscriptions={}, no broadcast)
--     │     — callable via @name prefix or orcs.request(fqn, "process", ...)
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
--   Config-based agents (settings.agents) are still loaded regardless of concierge.
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
--   @<agent_name> <msg>    → agent::<name> (dynamic, from settings.agents)
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
-- | agents            | table { [name] = agent_config }   | {}               | Config-based agent definitions     |
--
-- ### agents.<name> sub-table
--
-- Each key under `agents` defines a specialized CommonAgent instance.
-- The key becomes the agent name (used for @prefix routing and FQN).
--
-- | Key           | Type      | Default          | Description                    |
-- |---------------|-----------|------------------|--------------------------------|
-- | expertise     | string    | ""               | System prompt expertise block  |
-- | llm_provider  | string    | (inherit parent) | LLM provider override          |
-- | llm_model     | string    | (inherit parent) | Model override                 |
-- | llm_base_url  | string    | (inherit parent) | Provider base URL override     |
-- | llm_api_key   | string    | (inherit parent) | API key override               |
-- | subscriptions | string[]  | {}               | Event subscriptions (RPC-only) |
--
-- Example:
--   [components.settings.agent_mgr.agents.rust-reviewer]
--   expertise = "You are an expert Rust code reviewer."
--   llm_provider = "ollama"
--   llm_model = "llama3.2"
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

-- === Worker Scripts ===

-- CommonAgent: generic LLM-powered agent, spawned as an independent Component.
-- Has its own Lua VM, event loop, and ChannelRunner.
--
-- Parameterized via _agent_config (injected at spawn time):
--   name          — agent identifier (used as Component id)
--   expertise     — system prompt segment defining agent identity
--   subscriptions — EventBus subscriptions (default: {} = RPC-only)
--   llm           — LLM config overrides (provider, model, etc.)
--
-- Primary agent: _agent_config.subscriptions = {"AgentTask"} (broadcast receiver)
-- Specialized agents: _agent_config.subscriptions = {} (RPC-only, called via @prefix)
--
-- Output: calls orcs.output() directly (IO output channel inherited from parent).
-- RPC: "ping" operation for health check, "process" for LLM processing.
local common_agent_script = [[
-- _agent_config is injected by spawn_common_agent() before this script.
-- Fallback defaults for direct usage / testing:
if not _agent_config then
    _agent_config = { name = "common-agent", subscriptions = {"AgentTask"} }
end
local _name = _agent_config.name or "common-agent"

-- IntentDef registration guard: register skills/delegate/agent intents once per VM.
-- process() is called on every LLM turn; IntentDefs are idempotent but wasteful.
local _intents_registered = false

return {
    id = _name,
    namespace = "agent",
    subscriptions = _agent_config.subscriptions or {},

    -- operation="process" — main LLM processing (from AgentTask event or RPC)
    --   NOTE: When invoked via event (fire-and-forget), the return value of
    --   on_request is discarded by the runtime. Output and observability are
    --   handled inside emit_success() / orcs.output_with_level().
    -- operation="ping"    — lightweight connectivity check (from init RPC)
    on_request = function(request)
        local input = request.payload or {}
        local operation = request.operation or "process"

        -- Build LLM opts from config passed via event payload or RPC payload
        local function build_llm_opts(input)
            local opts = {}
            local llm_cfg = input.llm_config or {}
            if llm_cfg.provider       then opts.provider       = llm_cfg.provider end
            if llm_cfg.model          then opts.model           = llm_cfg.model end
            if llm_cfg.base_url       then opts.base_url        = llm_cfg.base_url end
            if llm_cfg.api_key        then opts.api_key         = llm_cfg.api_key end
            if llm_cfg.temperature    then opts.temperature     = llm_cfg.temperature end
            if llm_cfg.max_tokens     then opts.max_tokens      = llm_cfg.max_tokens end
            if llm_cfg.timeout        then opts.timeout         = llm_cfg.timeout end
            if llm_cfg.max_tool_turns then opts.max_tool_turns  = llm_cfg.max_tool_turns end
            return opts
        end

        -- Output LLM response to IO and emit observability event
        local function emit_success(message, llm_resp)
            orcs.output(llm_resp.content or "")
            orcs.emit_event("Extension", "llm_response", {
                message = message,
                response = llm_resp.content,
                session_id = llm_resp.session_id,
                cost = llm_resp.cost,
                source = _name,
            })
            orcs.log("debug", string.format(
                "%s: completed (cost=%.4f, session=%s)",
                _name, llm_resp.cost or 0, llm_resp.session_id or "none"
            ))
        end

        -- Ping: lightweight connectivity check (no tokens consumed)
        if operation == "ping" then
            local ping_opts = build_llm_opts(input)
            local ping_resp = orcs.llm_ping(ping_opts)
            return {
                success = ping_resp.ok,
                data = ping_resp,
            }
        end

        local message = input.message or ""

        -- Session resumption: skip full prompt assembly, send message directly
        if input.session_id and input.session_id ~= "" then
            orcs.log("debug", _name .. ": resuming session " .. input.session_id:sub(1, 20))
            local opts = build_llm_opts(input)
            opts.session_id = input.session_id
            opts.resolve = true
            opts.hil_intents = true  -- Propagate Suspended for HIL approval
            local llm_resp = orcs.llm(message, opts)
            if llm_resp and llm_resp.ok then
                emit_success(message, llm_resp)
                return {
                    success = true,
                    data = {
                        response = llm_resp.content,
                        session_id = llm_resp.session_id,
                        num_turns = llm_resp.num_turns,
                        cost = llm_resp.cost,
                        source = _name,
                    },
                }
            else
                local err = (llm_resp and llm_resp.error) or "llm resume failed"
                orcs.output_with_level("[" .. _name .. "] Error: " .. tostring(err), "error")
                return { success = false, error = err }
            end
        end

        -- First call: full prompt assembly
        local history_context = input.history_context or ""
        local delegation_context = input.delegation_context or ""
        local placement = input.prompt_placement or "both"

        -- 0a. Fetch foundation segments (system/task/guard from agent.md)
        local f_system = ""
        local f_task = ""
        local f_guard = ""
        local f_resp = orcs.request("foundation::foundation_manager", "get_all", {})
        if f_resp and f_resp.success and f_resp.data then
            f_system = f_resp.data.system or ""
            f_task = f_resp.data.task or ""
            f_guard = f_resp.data.guard or ""
        else
            orcs.log("debug", _name .. ": foundation segments unavailable, proceeding without")
        end

        -- 0b. Fetch console metrics (git, cwd, timestamp)
        local console_block = ""
        local m_resp = orcs.request("metrics::console_metrics", "get_all", {})
        if m_resp and m_resp.success and m_resp.formatted then
            console_block = m_resp.formatted
        else
            orcs.log("debug", _name .. ": console metrics unavailable, proceeding without")
        end

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
            -- Note: lua.to_value() (mlua serde) may place array elements in
            -- the hash part of the table, causing # to return 0.
            -- Use skills[1] existence check instead of #skills > 0.
            if type(skills) == "table" and skills[1] ~= nil then
                local lines = {}
                for _, s in ipairs(skills) do
                    skill_count = skill_count + 1
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

                -- Register recommended skills as IntentDefs for tool-use resolution
                local registered_names = {}
                for _, s in ipairs(skills) do
                    local reg = orcs.register_intent({
                        name = s.name,
                        description = (s.description or "") .. " [Skill: invoke to retrieve full instructions]",
                        component = "skill::skill_manager",
                        operation = "execute",
                        params = {
                            name = {
                                type = "string",
                                description = "Name of the skill to execute",
                                required = true,
                            },
                        },
                    })
                    if reg and reg.ok then
                        registered_names[#registered_names + 1] = s.name
                        orcs.log("debug", _name .. ": registered skill intent: " .. s.name)
                    else
                        local err_msg = (reg and reg.error) or (reg and tostring(reg)) or "nil returned"
                        orcs.log("warn", _name .. ": skill intent registration failed: " .. s.name
                            .. " (" .. err_msg .. ")")
                    end
                end
                if #registered_names > 0 then
                    orcs.log("info", string.format(
                        "%s: registered %d skill intent(s): [%s]",
                        _name, #registered_names, table.concat(registered_names, ", ")
                    ))
                elseif skill_count > 0 then
                    orcs.log("warn", string.format(
                        "%s: all %d skill intent registration(s) failed",
                        _name, skill_count
                    ))
                end
            end
        end

        -- Register static IntentDefs (delegate_task + agent intents) once per VM.
        -- These definitions are fixed after init and don't change across LLM turns.
        -- Skill IntentDefs (above) are NOT guarded because recommended skills vary per turn.
        if not _intents_registered then
            -- Register delegate_task intent: allows LLM to delegate work to a sub-agent.
            -- The IntentDef routes to agent_mgr, which spawns/dispatches to delegate-worker.
            local delegate_reg = orcs.register_intent({
                name = "delegate_task",
                description = "Delegate a task to an independent sub-agent that runs its own LLM session with tool access. "
                    .. "Use when the task requires separate investigation, research, or multi-step work "
                    .. "that would benefit from independent processing. The sub-agent works asynchronously; "
                    .. "results appear in your context on the next turn.",
                component = "builtin::agent_mgr",
                operation = "delegate",
                timeout_ms = 600000,
                params = {
                    description = {
                        type = "string",
                        description = "Detailed description of the task to delegate",
                        required = true,
                    },
                    context = {
                        type = "string",
                        description = "Relevant context for the sub-agent (file paths, constraints, background)",
                        required = false,
                    },
                },
            })
            if delegate_reg and delegate_reg.ok then
                orcs.log("debug", _name .. ": registered delegate_task intent")
            else
                orcs.log("warn", _name .. ": delegate_task intent registration failed: "
                    .. ((delegate_reg and delegate_reg.error) or "unknown"))
            end

            -- Register IntentDefs for registered agents (from payload).
            -- Allows LLM to invoke specialized agents as tools.
            -- Routes to agent_mgr "invoke_agent" operation, which spawns on-demand.
            local registered_agent_names = {}
            for _, agent in ipairs(input.registered_agents or {}) do
                local agent_name = agent.name or ""
                if agent_name ~= "" then
                    local agent_reg = orcs.register_intent({
                        name = "invoke_" .. agent_name,
                        description = (agent.expertise ~= "" and agent.expertise)
                            or ("Invoke the " .. agent_name .. " agent"),
                        component = "builtin::agent_mgr",
                        operation = "invoke_agent",
                        timeout_ms = 600000,
                        params = {
                            agent_name = {
                                type = "string",
                                description = "Name of the agent to invoke",
                                required = true,
                            },
                            message = {
                                type = "string",
                                description = "Task or message to send to the agent",
                                required = true,
                            },
                        },
                    })
                    if agent_reg and agent_reg.ok then
                        registered_agent_names[#registered_agent_names + 1] = agent_name
                    else
                        orcs.log("warn", _name .. ": agent intent registration failed: " .. agent_name
                            .. " (" .. ((agent_reg and agent_reg.error) or "unknown") .. ")")
                    end
                end
            end
            if #registered_agent_names > 0 then
                orcs.log("info", string.format(
                    "%s: registered %d agent intent(s): [%s]",
                    _name, #registered_agent_names, table.concat(registered_agent_names, ", ")
                ))
            end

            _intents_registered = true
        end

        local tool_desc = ""
        if orcs.tool_descriptions then
            local td = orcs.tool_descriptions()
            if td and td ~= "" then
                tool_desc = "## Available ORCS Tools\n" .. td
            end
        end

        -- 2. Compose system block (skills + tool descriptions)
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

        -- 3b. Compose delegation results block (from completed sub-agent tasks)
        local delegation_block = ""
        if delegation_context ~= "" then
            delegation_block = delegation_context
        end

        -- 4. Assemble prompt based on placement strategy
        --
        -- Fixed positions:
        --   f_system      → always at top (identity/core rules)
        --   expertise     → after f_system (agent identity)
        --   console_block → after expertise (workspace context)
        --   f_task        → before history (project context)
        --   f_guard       → just before user message (output constraints)
        --
        -- Placement strategy controls skill/tool block (system_full) position only.
        local sections = {}

        -- Foundation:system — always top
        if f_system ~= "" then sections[#sections + 1] = f_system end

        -- Agent expertise — defines agent identity (injected via _agent_config)
        local expertise = _agent_config.expertise or ""
        if expertise ~= "" then
            sections[#sections + 1] = "## Expertise\n" .. expertise
        end

        -- Console metrics — after agent identity
        if console_block ~= "" then sections[#sections + 1] = console_block end

        if placement == "top" then
            -- [f:system] [Metrics] [Skills/Tools] [f:task] [History] [Delegation] [f:guard] [UserInput]
            if system_full ~= "" then sections[#sections + 1] = system_full end
            if f_task ~= "" then sections[#sections + 1] = f_task end
            if history_block ~= "" then sections[#sections + 1] = history_block end
            if delegation_block ~= "" then sections[#sections + 1] = delegation_block end
            if f_guard ~= "" then sections[#sections + 1] = f_guard end
            sections[#sections + 1] = message

        elseif placement == "bottom" then
            -- [f:system] [Metrics] [f:task] [History] [Delegation] [f:guard] [UserInput] [Skills/Tools]
            if f_task ~= "" then sections[#sections + 1] = f_task end
            if history_block ~= "" then sections[#sections + 1] = history_block end
            if delegation_block ~= "" then sections[#sections + 1] = delegation_block end
            if f_guard ~= "" then sections[#sections + 1] = f_guard end
            sections[#sections + 1] = message
            if system_full ~= "" then sections[#sections + 1] = system_full end

        else
            -- "both" (default):
            -- [f:system] [Metrics] [Skills/Tools] [f:task] [History] [Delegation] [Skills/Tools] [f:guard] [UserInput]
            if system_full ~= "" then sections[#sections + 1] = system_full end
            if f_task ~= "" then sections[#sections + 1] = f_task end
            if history_block ~= "" then sections[#sections + 1] = history_block end
            if delegation_block ~= "" then sections[#sections + 1] = delegation_block end
            if system_full ~= "" then sections[#sections + 1] = system_full end
            if f_guard ~= "" then sections[#sections + 1] = f_guard end
            sections[#sections + 1] = message
        end

        local prompt = table.concat(sections, "\n\n")

        orcs.log("debug", string.format(
            "%s: prompt built (placement=%s, skills=%d, history=%d, tools=%d, foundation=%d+%d+%d, metrics=%d, expertise=%d chars)",
            _name, placement, skill_count, #history_context, #tool_desc,
            #f_system, #f_task, #f_guard, #console_block, #expertise
        ))

        -- 5. Call LLM via orcs.llm() with provider/model from config
        -- Note: session_id is handled by the early-return path above;
        -- this path is always a fresh first call.
        local llm_opts = build_llm_opts(input)
        -- Enable tool-use auto-resolution: LLM tool_use calls (skills, builtins)
        -- are dispatched via IntentRegistry and results fed back automatically.
        llm_opts.resolve = true
        llm_opts.hil_intents = true  -- Propagate Suspended for HIL approval
        -- Apply _agent_config.llm overrides (agent-specific LLM settings).
        -- Unconditional overwrite: keys in _agent_config.llm are explicitly
        -- specified by the user (config.toml agents.<name>.llm_*), so they
        -- take priority over the parent agent_mgr settings.
        --
        -- NOTE: This is static config-time override only. If a future feature
        -- allows the Mgr to dynamically switch models at runtime (e.g. "this
        -- task is simple, use Sonnet 4.5 instead"), that path MUST go through
        -- HIL (Human-in-the-Loop) approval before overwriting agent config.
        if _agent_config.llm then
            for k, v in pairs(_agent_config.llm) do
                llm_opts[k] = v
            end
        end

        local llm_resp = orcs.llm(prompt, llm_opts)
        if llm_resp and llm_resp.ok then
            emit_success(message, llm_resp)
            return {
                success = true,
                data = {
                    response = llm_resp.content,
                    session_id = llm_resp.session_id,
                    num_turns = llm_resp.num_turns,
                    cost = llm_resp.cost,
                    source = _name,
                },
            }
        else
            local err = (llm_resp and llm_resp.error) or "llm call failed"
            orcs.output_with_level("[" .. _name .. "] Error: " .. tostring(err), "error")
            return {
                success = false,
                error = err,
            }
        end
    end,

    on_signal = function(signal)
        if signal.kind == "Veto" then
            return "Abort"
        end
        return "Handled"
    end,
}
]]

-- === Agent Registry & Lifecycle ===
--
-- Lifecycle:
--   [1] Register  — store agent definition in registry (init time, from config.toml)
--   [2] Spawn     — Mgr creates a running instance from a registered definition
--   [3] Spawned   — instance is running, tracked in spawned_agents
--   [4] Complete  — task done, instance removed (or persistent: stays alive)
--
-- Registered agents are templates (name, expertise, llm overrides).
-- Spawned agents are running instances created from those templates.
-- Multiple instances of the same registered agent can coexist.
-- Persistent agents (primary, delegate-worker) are spawned at init and stay alive.

--- Agent registry: name → config (definitions/templates from config.toml).
--- Populated by register_agent(), queried on-demand by @prefix routing.
local agent_registry = {}

--- Spawned agents: channel_id → { name, fqn, channel_id, config, persistent }.
--- Keyed by channel_id (Rust ChannelId UUID, unique and recovery-safe).
--- Use find_spawned_by_name() for name-based lookup.
local spawned_agents = {}

--- Reverse index: name → channel_id.
--- Maintained by spawn_agent() for O(1) name-based lookup.
--- spawn_registered_agent() reuses existing instances, so same-name overwrites are safe.
local spawned_by_name = {}

--- Deep-copy a value recursively (tables are copied, non-tables passed through).
--- Used to isolate registry definitions from spawned instance mutations.
--- @param t any  Value to copy
--- @return any  Deep-copied value
local function deep_copy(t)
    if type(t) ~= "table" then return t end
    local copy = {}
    for k, v in pairs(t) do
        copy[k] = deep_copy(v)
    end
    return copy
end

--- Register an agent definition in the registry.
--- Does NOT spawn the agent — just stores the config for on-demand use.
--- @param name string  Agent name (used as @prefix and registry key)
--- @param config table  Agent config: { expertise?, llm?, subscriptions? }
--- @return boolean, string|nil  success, error message
local function register_agent(name, config)
    if not name or name == "" then
        return false, "agent name is required"
    end
    if agent_registry[name] then
        return false, "duplicate agent registration: " .. name
    end
    config.name = name
    agent_registry[name] = config
    orcs.log("info", "registered agent '" .. name .. "'")
    return true
end

--- Find a spawned agent by name via reverse index (O(1)).
--- Used by @prefix routing and invoke_agent.
--- @param name string  Agent name to look up
--- @return table|nil  Spawned agent info { name, fqn, channel_id, config, persistent }
local function find_spawned_by_name(name)
    local ch_id = spawned_by_name[name]
    if ch_id then return spawned_agents[ch_id] end
    return nil
end

--- Spawn a CommonAgent instance.
--- Instance is keyed by channel_id (Rust ChannelId UUID) for recovery safety.
--- @param config table  Config with name (required), expertise, subscriptions, llm
--- @param opts table|nil  Optional overrides (not yet used; reserved for ad-hoc prompt)
--- @return table|nil, string|nil  spawn result, error message
local function spawn_agent(config, opts)
    if not config or not config.name then
        return nil, "config.name is required"
    end

    -- Merge opts if provided (future: ad-hoc prompt injection)
    if opts then
        for k, v in pairs(opts) do
            if config[k] == nil then
                config[k] = v
            end
        end
    end

    local result = orcs.spawn_runner({
        script = common_agent_script,
        id = config.name,
        globals = { _agent_config = config },
    })
    if not result then
        return nil, "spawn_runner returned nil for agent '" .. config.name .. "'"
    end
    if result.ok then
        local ch_id = result.channel_id
        spawned_agents[ch_id] = {
            name = config.name,
            channel_id = ch_id,
            fqn = result.fqn,
            config = config,
            persistent = config.persistent or false,
        }
        spawned_by_name[config.name] = ch_id  -- Reverse index for O(1) lookup
        orcs.log("info", string.format(
            "spawned agent '%s' (channel=%s, fqn=%s)", config.name, ch_id, result.fqn
        ))
    end
    return result, (not result.ok) and result.error or nil
end

--- Spawn a registered agent by name (on-demand).
--- Looks up the registry, copies the config, and spawns.
--- If an instance with the same name is already running, returns it.
--- @param name string  Registered agent name
--- @return table|nil, string|nil  spawn result, error message
local function spawn_registered_agent(name)
    local reg = agent_registry[name]
    if not reg then
        return nil, "agent not registered: " .. name
    end
    local existing = find_spawned_by_name(name)
    if existing then
        -- Already running — return existing info
        return { ok = true, fqn = existing.fqn }
    end
    -- Deep-copy config so spawn doesn't mutate registry
    local config = deep_copy(reg)
    return spawn_agent(config)
end

--- List registered agents (definitions).
--- @return table  Array of { name, expertise, has_llm_override, spawned }
local function list_registered_agents()
    local result = {}
    for name, config in pairs(agent_registry) do
        result[#result + 1] = {
            name = name,
            expertise = config.expertise or "",
            has_llm_override = config.llm ~= nil,
            spawned = find_spawned_by_name(name) ~= nil,
        }
    end
    table.sort(result, function(a, b) return a.name < b.name end)
    return result
end

--- List spawned (running) agent instances.
--- @return table  Array of { channel_id, name, fqn, persistent }
local function list_spawned_agents()
    local result = {}
    for ch_id, info in pairs(spawned_agents) do
        result[#result + 1] = {
            channel_id = ch_id,
            name = info.name,
            fqn = info.fqn,
            persistent = info.persistent,
        }
    end
    table.sort(result, function(a, b)
        if a.name ~= b.name then return a.name < b.name end
        return a.channel_id < b.channel_id
    end)
    return result
end

-- === Constants & Settings ===

local HISTORY_LIMIT = 10  -- Max recent conversation entries to include as context
local MAX_AGENT_INTENTS = 3  -- Max agent IntentDefs exposed to LLM per dispatch

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

--- Set of agent names authorized for LLM invocation via IntentDef.
--- Updated by dispatch_llm() when building registered_agents_for_payload.
--- Checked by invoke_agent handler to reject unauthorized agent_name values.
local intent_allowed_agents = {}

-- Mapping from component_settings keys (llm_*) to LLM config keys (without prefix).
-- Single source of truth: used by dispatch_llm(), delegate operation, and init() ping.
local LLM_KEY_MAP = {
    llm_provider       = "provider",
    llm_model          = "model",
    llm_base_url       = "base_url",
    llm_api_key        = "api_key",
    llm_temperature    = "temperature",
    llm_max_tokens     = "max_tokens",
    llm_timeout        = "timeout",
    llm_max_tool_turns = "max_tool_turns",
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
    -- [%w_%-] matches alphanumeric, underscore, and hyphen (e.g. "rust-reviewer")
    local prefix, rest = message:match("^@([%w_%-]+)%s+(.+)$")
    if prefix then
        return prefix:lower(), rest
    end
    -- Also match bare @prefix without args
    local bare = message:match("^@([%w_%-]+)%s*$")
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
--- Note: equivalent logic exists in Rust (component.rs truncate_utf8).
--- Duplicated here because Lua cannot call the Rust helper directly.
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

    -- Liveness check: non-blocking channel probe via orcs.is_alive().
    -- Zero-cost (no RPC roundtrip) — checks if the runner's event channel is open.
    -- Guard: orcs.is_alive may not exist on older runtimes without supervision support.
    if orcs.is_alive and not orcs.is_alive(concierge_fqn) then
        orcs.log("error", "concierge '" .. concierge_fqn .. "' channel is closed (runner stopped)")
        orcs.output_with_level(
            "[AgentMgr] concierge '" .. concierge_fqn
            .. "' is no longer running. LLM dispatch disabled.",
            "error"
        )
        concierge_fqn = nil
        return { success = false, error = "concierge became unreachable" }
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

    -- Build registered agents list for CommonAgent IntentDef registration.
    -- Limited to MAX_AGENT_INTENTS to control LLM tool count and token usage.
    -- Also updates intent_allowed_agents set for invoke_agent validation.
    local registered_agents_for_payload = {}
    intent_allowed_agents = {}  -- Reset on each dispatch (reflects latest registry)
    local reg_list = list_registered_agents()
    for i = 1, math.min(#reg_list, MAX_AGENT_INTENTS) do
        registered_agents_for_payload[i] = reg_list[i]
        intent_allowed_agents[reg_list[i].name] = true
    end

    -- Emit AgentTask event (fire-and-forget, non-blocking).
    -- The concierge agent receives this and processes asynchronously.
    -- Note: agent_status is provided by console_metrics (via list_agents RPC),
    -- not in this payload. registered_agents is for IntentDef registration only.
    local payload = {
        message = message,
        prompt_placement = placement,
        llm_config = llm_config,
        history_context = history_context,
        delegation_context = delegation_context,
        registered_agents = registered_agents_for_payload,
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

                -- Notify user that the delegated task finished.
                if payload.success then
                    orcs.output("[AgentMgr] Delegate " .. request_id .. " completed.")
                else
                    orcs.output_with_level(
                        "[AgentMgr] Delegate " .. request_id .. " failed: "
                            .. (payload.error or "unknown"),
                        "error"
                    )
                end

                -- Auto-dispatch to concierge so it can report the delegation result.
                -- dispatch_llm() calls fetch_delegation_context() which picks up
                -- the just-stored result, then emits AgentTask (fire-and-forget).
                local status = payload.success and "completed" or "failed"
                dispatch_llm("[Delegate " .. request_id .. " " .. status
                    .. "] Review the delegation result and briefly report to the user.")
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
        -- Spawns a per-delegation worker for parallel execution.
        --
        -- Each delegation gets its own Lua VM (via spawn_runner) that subscribes
        -- to a unique "DelegateTask-{request_id}" event. Multiple delegations
        -- issued in the same LLM turn run concurrently in independent VMs.
        if operation == "delegate" then
            local payload = request.payload or {}
            local description = payload.description or ""
            if description == "" then
                return { success = false, error = "delegate_task requires a description" }
            end

            -- Generate unique request ID
            delegation_counter = delegation_counter + 1
            local request_id = string.format("d%03d", delegation_counter)

            -- Extract LLM config for the delegate
            local llm_config = extract_llm_config()

            -- Resolve delegate_backend FQN (independent from concierge)
            local delegate_backend = component_settings.delegate_backend
            if delegate_backend == "" then delegate_backend = nil end

            local task_payload = {
                request_id = request_id,
                description = description,
                context = payload.context or "",
                llm_config = llm_config,
                delegate_backend = delegate_backend,
            }

            -- Spawn a dedicated worker per delegation (parallel execution).
            -- The worker reads _delegate_payload to determine its unique event
            -- subscription ("DelegateTask-{request_id}") and component ID.
            local worker = orcs.spawn_runner({
                builtin = "delegate_worker.lua",
                id = "delegate-" .. request_id,
                globals = { _delegate_payload = task_payload },
            })

            if not worker or not worker.ok then
                local spawn_err = (worker and worker.error) or "unknown"
                orcs.output_with_level(
                    "[AgentMgr] Error: failed to spawn delegate worker for " .. request_id,
                    "error"
                )
                return { success = false, error = "failed to spawn delegate worker: " .. spawn_err }
            end

            -- Emit targeted event to the specific worker (fire-and-forget).
            -- Event kind includes request_id so only this worker receives it.
            local event_kind = "DelegateTask-" .. request_id
            local delivered = orcs.emit_event(event_kind, "process", task_payload)

            if not delivered then
                orcs.output_with_level(
                    "[AgentMgr] Error: DelegateTask event not delivered to worker " .. request_id,
                    "error"
                )
                return { success = false, error = "DelegateTask event delivery failed" }
            end

            orcs.log("info", string.format(
                "AgentMgr: delegated task %s to dedicated worker (fqn=%s, %d chars)",
                request_id, worker.fqn, #description
            ))

            return {
                success = true,
                data = {
                    request_id = request_id,
                    worker_fqn = worker.fqn,
                    message = "Task delegated to sub-agent (id: " .. request_id .. "). Results will appear in your context on the next turn.",
                },
            }
        end

        -- Handle agent invocation from LLM IntentDef dispatch.
        -- Spawns registered agent on-demand and forwards the message via RPC.
        -- Validates agent_name against intent_allowed_agents (set by dispatch_llm)
        -- to prevent LLM from invoking agents not exposed via IntentDef.
        if operation == "invoke_agent" then
            local payload = request.payload or {}
            local agent_name = payload.agent_name or ""
            local agent_message = payload.message or ""
            if agent_name == "" then
                return { success = false, error = "invoke_agent requires agent_name" }
            end
            if not intent_allowed_agents[agent_name] then
                orcs.log("warn", "invoke_agent: rejected unauthorized agent_name '" .. agent_name .. "'")
                return { success = false, error = "agent '" .. agent_name .. "' is not authorized for LLM invocation" }
            end
            if agent_message == "" then
                return { success = false, error = "invoke_agent requires message" }
            end

            -- Spawn from registry (or reuse existing)
            local spawn_result, spawn_err = spawn_registered_agent(agent_name)
            if not spawn_result or not spawn_result.ok then
                return { success = false, error = spawn_err or "agent spawn failed" }
            end

            local agent_info = find_spawned_by_name(agent_name)
            if not agent_info then
                return { success = false, error = "agent spawned but not tracked" }
            end

            -- Forward message to the spawned agent via RPC.
            -- Timeout: llm_timeout (seconds, from config.toml) → ms, default 120s.
            local rpc_timeout = (component_settings.llm_timeout or 120) * 1000
            local resp = orcs.request(agent_info.fqn, "process", {
                message = agent_message,
                llm_config = extract_llm_config(),
                prompt_placement = component_settings.prompt_placement or "both",
            }, { timeout_ms = rpc_timeout })
            if resp and resp.success then
                return {
                    success = true,
                    data = {
                        response = resp.data,
                        source = "agent:" .. agent_name,
                    },
                }
            else
                local err = (resp and resp.error) or "agent request failed"
                return { success = false, error = err }
            end
        end

        -- List registered and spawned agents.
        if operation == "list_agents" then
            return {
                success = true,
                data = {
                    registered = list_registered_agents(),
                    spawned = list_spawned_agents(),
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
            end

            -- Dynamic routing: check spawned agents first, then registry
            local agent_info = find_spawned_by_name(prefix)
            if not agent_info and agent_registry[prefix] then
                -- On-demand spawn from registry
                orcs.log("info", "AgentMgr: on-demand spawn for @" .. prefix)
                local spawn_result, spawn_err = spawn_registered_agent(prefix)
                if spawn_result and spawn_result.ok then
                    agent_info = find_spawned_by_name(prefix)
                else
                    orcs.output_with_level(
                        "[AgentMgr] @" .. prefix .. " spawn failed: " .. (spawn_err or "unknown"),
                        "error"
                    )
                    return { success = false, error = spawn_err or "agent spawn failed" }
                end
            end
            if agent_info then
                orcs.log("info", "AgentMgr routing @" .. prefix .. " -> " .. agent_info.fqn)
                -- Timeout: llm_timeout (seconds, from config.toml) → ms, default 120s.
                local rpc_timeout = (component_settings.llm_timeout or 120) * 1000
                local resp = orcs.request(agent_info.fqn, "process", {
                    message = body,
                    llm_config = extract_llm_config(),
                    prompt_placement = component_settings.prompt_placement or "both",
                }, { timeout_ms = rpc_timeout })
                if resp and resp.success then
                    return {
                        success = true,
                        data = {
                            response = resp.data,
                            source = "agent:" .. prefix,
                        },
                    }
                else
                    local err = (resp and resp.error) or "agent request failed"
                    orcs.output_with_level("[AgentMgr] @" .. prefix .. " error: " .. err, "error")
                    return { success = false, error = err }
                end
            end

            -- Unknown @prefix: pass full message to concierge as-is
            orcs.log("debug", "AgentMgr: unknown prefix @" .. prefix .. ", falling through to concierge")
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
            -- External concierge: verify the component is reachable before trusting the FQN.
            -- Without this guard, a stale or misconfigured FQN causes AgentTask events to
            -- silently vanish (emit_event returns delivered=true due to self-channel counting).
            orcs.log("info", "concierge='" .. concierge .. "': verifying external agent...")
            local probe_ok, probe_err = pcall(function()
                local resp = orcs.request(concierge, "ping", {}, { timeout_ms = 5000 })
                if resp and resp.success then
                    concierge_fqn = concierge
    
                    orcs.log("info", "concierge='" .. concierge .. "': external agent verified")
                else
                    local err_msg = (resp and resp.error) or "no response"
                    orcs.log("error", string.format(
                        "concierge='%s': external agent unreachable (%s). "
                        .. "LLM dispatch will be disabled until a valid concierge is available.",
                        concierge, err_msg
                    ))
                    orcs.output_with_level(
                        "[AgentMgr] external concierge '" .. concierge
                        .. "' is not responding. Check config or component status.",
                        "warn"
                    )
                    -- concierge_fqn remains nil → dispatch_llm will return an error
                end
            end)
            if not probe_ok then
                orcs.log("error", "concierge probe failed: " .. tostring(probe_err))
                orcs.output_with_level(
                    "[AgentMgr] external concierge '" .. concierge
                    .. "' probe threw an error. LLM dispatch disabled.",
                    "warn"
                )
                -- concierge_fqn remains nil
            end
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

        -- Register config-based agents from [components.settings.agent_mgr.agents]
        -- Each key is the agent name, value is { expertise?, llm_provider?, llm_model?, ... }
        -- Agents are registered (not spawned) — spawned on-demand when @prefix is used.
        local agents_registered, agents_failed = 0, 0
        local agents_config = component_settings.agents
        if agents_config and type(agents_config) == "table" then
            for name, agent_cfg in pairs(agents_config) do
                if type(agent_cfg) == "table" then
                    -- Build registration config from TOML data
                    local reg_cfg = {}
                    if agent_cfg.expertise then
                        reg_cfg.expertise = tostring(agent_cfg.expertise)
                    end
                    if agent_cfg.subscriptions then
                        reg_cfg.subscriptions = agent_cfg.subscriptions
                    end
                    -- Collect LLM overrides into reg_cfg.llm
                    local llm_overrides = {}
                    local has_llm = false
                    for _, key in ipairs({"llm_provider", "llm_model", "llm_base_url", "llm_api_key"}) do
                        if agent_cfg[key] then
                            llm_overrides[key:sub(5)] = agent_cfg[key]
                            has_llm = true
                        end
                    end
                    if has_llm then
                        reg_cfg.llm = llm_overrides
                    end

                    local ok, reg_err = register_agent(name, reg_cfg)
                    if ok then
                        agents_registered = agents_registered + 1
                    else
                        agents_failed = agents_failed + 1
                        orcs.log("warn", "agent registration failed: " .. name
                            .. " (" .. (reg_err or "unknown") .. ")")
                    end
                else
                    agents_failed = agents_failed + 1
                    orcs.log("warn", "agent config invalid: " .. name
                        .. " (expected table, got " .. type(agent_cfg) .. ")")
                end
            end
        end
        if agents_registered > 0 or agents_failed > 0 then
            orcs.log("info", string.format(
                "config-based agents: %d registered, %d failed", agents_registered, agents_failed
            ))
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
        local agents_status = agents_registered > 0 and tostring(agents_registered) or "none"
        orcs.output("[AgentMgr] Ready (backend: " .. backend
            .. ", delegate: " .. delegate_status
            .. ", agents: " .. agents_status .. ")")
        orcs.log("info", "agent_mgr initialized")
    end,

    shutdown = function()
        orcs.log("info", "agent_mgr shutdown")
    end,
}

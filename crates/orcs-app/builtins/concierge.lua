-- concierge.lua
-- Default concierge agent: handles AgentTask events with built-in LLM backend.
--
-- The concierge is the primary agent responsible for processing user messages.
-- It assembles prompts (foundation, metrics, skills, history, delegation context),
-- registers IntentDefs (delegate_task, recommended skills), and calls orcs.llm()
-- with tool-use auto-resolution.
--
-- Operations:
--   process  — main LLM processing (from AgentTask event)
--   status   — return current state for observability
--   ping     — lightweight LLM provider connectivity check
--
-- This is a spawned concierge agent (child of agent_mgr).
-- It runs in its own Lua VM with an independent event loop.

-- === Module State ===

-- Configurable timeouts (overridden via globals injected by agent_mgr)
local delegate_timeout_ms = (_delegate_timeout_ms and type(_delegate_timeout_ms) == "number")
    and _delegate_timeout_ms
    or 600000  -- 10 minutes default

local concierge_timeout_ms = (_concierge_timeout_ms and type(_concierge_timeout_ms) == "number")  -- luacheck: ignore 113
    and _concierge_timeout_ms  -- luacheck: ignore 113
    or 600000  -- 10 minutes default

local busy = false
local session_id = nil
local turn_count = 0
local last_cost = nil
local last_provider = nil
local last_model = nil

-- === Helpers ===

--- Build LLM opts from config passed via event payload or RPC payload.
--- Pure function: no side effects on module state.
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
    -- Wall-clock timeout for the entire resolve loop
    if concierge_timeout_ms > 0 then
        opts.overall_timeout = math.floor(concierge_timeout_ms / 1000)
    end
    return opts
end

--- Update module tracking state after a successful LLM call.
--- Called explicitly (not as a side effect of build_llm_opts) to avoid
--- overwriting valid state with nil when input.llm_config is absent
--- (e.g., session resumption without config re-specification).
local function update_tracking(input, llm_resp)
    session_id = llm_resp.session_id
    last_cost = llm_resp.cost
    local llm_cfg = input.llm_config or {}
    if llm_cfg.provider then last_provider = llm_cfg.provider end
    if llm_cfg.model then last_model = llm_cfg.model end
end

--- Output LLM response to IO and emit observability event.
local function emit_success(message, llm_resp)
    if llm_resp.stop_reason == "max_tokens" then
        orcs.log("warn", string.format(
            "concierge: response truncated by max_tokens (session=%s, content_len=%d)",
            (llm_resp.session_id or "none"):sub(1, 12),
            #(llm_resp.content or "")
        ))
        orcs.output_with_level(
            "[Concierge] Warning: response was truncated due to output token limit. "
            .. "The result may be incomplete.",
            "warn"
        )
    end
    orcs.output("[Concierge] " .. (llm_resp.content or ""))
    orcs.emit_event("Extension", "llm_response", {
        message = message,
        response = llm_resp.content,
        session_id = llm_resp.session_id,
        cost = llm_resp.cost,
        stop_reason = llm_resp.stop_reason,
        source = "concierge",
    })
    orcs.log("info", string.format(
        "concierge: completed (cost=%.4f, session=%s)",
        llm_resp.cost or 0, (llm_resp.session_id or "none"):sub(1, 12)
    ))
end

-- === Prompt Assembly ===

--- Fetch foundation segments (system/task/guard from agent.md).
local function fetch_foundation()
    local f_resp = orcs.request("foundation::foundation_manager", "get_all", {})
    if f_resp and f_resp.success and f_resp.data then
        return f_resp.data.system or "", f_resp.data.task or "", f_resp.data.guard or ""
    end
    orcs.log("info", "concierge: foundation segments unavailable, proceeding without")
    return "", "", ""
end

--- Fetch console metrics (git, cwd, timestamp).
local function fetch_console_metrics()
    local m_resp = orcs.request("metrics::console_metrics", "get_all", {})
    if m_resp and m_resp.success and m_resp.formatted then
        return m_resp.formatted
    end
    orcs.log("info", "concierge: console metrics unavailable, proceeding without")
    return ""
end

--- Fetch skill recommendations and register as IntentDefs.
--- Returns formatted recommendation text and skill count.
local function fetch_and_register_skills(message, history_context)
    local recommendation = ""
    local skill_count = 0

    local rec_resp = orcs.request("skill::skill_manager", "recommend", {
        intent = message,
        context = history_context,
        limit = 5,
    })
    if not (rec_resp and rec_resp.success and rec_resp.data) then
        return recommendation, skill_count
    end

    local skills = rec_resp.data
    -- Note: lua.to_value() (mlua serde) may place array elements in
    -- the hash part of the table, causing # to return 0.
    -- Use skills[1] existence check instead of #skills > 0.
    if type(skills) ~= "table" or skills[1] == nil then
        return recommendation, skill_count
    end

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
            orcs.log("debug", "concierge: registered skill intent: " .. s.name)
        else
            local err_msg = (reg and reg.error) or (reg and tostring(reg)) or "nil returned"
            orcs.log("warn", "concierge: skill intent registration failed: " .. s.name
                .. " (" .. err_msg .. ")")
        end
    end
    if #registered_names > 0 then
        orcs.log("info", string.format(
            "concierge: registered %d skill intent(s): [%s]",
            #registered_names, table.concat(registered_names, ", ")
        ))
    elseif skill_count > 0 then
        orcs.log("warn", string.format(
            "concierge: all %d skill intent registration(s) failed",
            skill_count
        ))
    end

    return recommendation, skill_count
end

--- Register delegate_task intent for LLM-initiated sub-agent delegation.
local function register_delegate_intent()
    local delegate_reg = orcs.register_intent({
        name = "delegate_task",
        description = "Delegate a task to an independent sub-agent that runs its own LLM session with tool access. "
            .. "Use when the task requires separate investigation, research, or multi-step work "
            .. "that would benefit from independent processing. The sub-agent works asynchronously; "
            .. "results appear in your context on the next turn.",
        component = "builtin::agent_mgr",
        operation = "delegate",
        timeout_ms = delegate_timeout_ms,
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
        orcs.log("info", "concierge: registered delegate_task intent")
    else
        orcs.log("warn", "concierge: delegate_task intent registration failed: "
            .. ((delegate_reg and delegate_reg.error) or "unknown"))
    end
end

--- Assemble the full prompt with placement strategy.
---
--- placement: "top" | "bottom" | "both" (default)
---
--- Design rationale for "both" (default):
---   System/Metrics/Skills are typically a few thousand tokens total.
---   History and data context can reach ~100K tokens.
---   LLMs exhibit "lost in the middle" attention bias — after 100K tokens of
---   history the model effectively forgets earlier instructions.
---
---   "both" literally places the full system context (f:system, Metrics,
---   Skills/Tools) at BOTH top and bottom, sandwiching the long history.
---   The bottom anchor acts as a re-grounding signal: "from here on is your
---   embedded context — focus here." Metrics (which include active agents,
---   provider state) are especially important to repeat since the model must
---   know its current operational state when generating the response.
---
---   NOTE: The duplication of system context is INTENTIONAL — it is the core
---   mechanism that prevents LLM attention degradation over long contexts.
---   The token cost increase (~2x system context) is a deliberate trade-off
---   for maintaining instruction adherence.
---   If future LLM architectures resolve the "lost in the middle" problem,
---   revisit this strategy and consider switching to "top" as the default.
---
--- Layout ("both"):
---   ┌── Top anchor ──────────────────────────────────────────────┐
---   │ [f:system] [Metrics] [Skills/Tools] [f:task]              │
---   ├── Long context (can be ~100K tokens) ──────────────────────┤
---   │ [History]                                                  │
---   ├── Bottom anchor (re-grounding) ────────────────────────────┤
---   │ [f:system] [Metrics] [Skills/Tools] [f:task]              │
---   │ [Delegation] [f:guard] [UserMessage]                      │
---   └────────────────────────────────────────────────────────────┘
---
--- TODO: "auto" mode planned — auto(both_cond=N) dynamically selects "both"
---   when estimated total context exceeds N tokens, otherwise falls back to "top"
---   to avoid redundant token spend on short conversations.
local function assemble_prompt(params)
    local message = params.message
    local placement = params.placement or "both"
    local f_system = params.f_system or ""
    local f_task = params.f_task or ""
    local f_guard = params.f_guard or ""
    local console_block = params.console_block or ""
    local system_full = params.system_full or ""
    local history_block = params.history_block or ""
    local delegation_block = params.delegation_block or ""

    local sections = {}

    if placement == "top" then
        -- Single anchor at top: all system context first, then data, then message
        if f_system ~= "" then sections[#sections + 1] = f_system end
        if console_block ~= "" then sections[#sections + 1] = console_block end
        if system_full ~= "" then sections[#sections + 1] = system_full end
        if f_task ~= "" then sections[#sections + 1] = f_task end
        if history_block ~= "" then sections[#sections + 1] = history_block end
        if delegation_block ~= "" then sections[#sections + 1] = delegation_block end
        if f_guard ~= "" then sections[#sections + 1] = f_guard end
        sections[#sections + 1] = message

    elseif placement == "bottom" then
        -- Single anchor at bottom: data (task+history) first, then all system
        -- context clustered near the user message for maximum attention weight.
        --
        -- Design: f:system is intentionally placed AFTER history (not at the
        -- absolute top). This keeps identity/rules close to the generation
        -- point, maximizing their influence on the response. f:task precedes
        -- history because it provides the project frame for interpreting it.
        if f_task ~= "" then sections[#sections + 1] = f_task end
        if history_block ~= "" then sections[#sections + 1] = history_block end
        if f_system ~= "" then sections[#sections + 1] = f_system end
        if console_block ~= "" then sections[#sections + 1] = console_block end
        if system_full ~= "" then sections[#sections + 1] = system_full end
        if delegation_block ~= "" then sections[#sections + 1] = delegation_block end
        if f_guard ~= "" then sections[#sections + 1] = f_guard end
        sections[#sections + 1] = message

    else
        -- "both" (default): full system context at BOTH top and bottom,
        -- sandwiching the long history. See design rationale above.

        -- === Top anchor: identity + state + tools + task ===
        if f_system ~= "" then sections[#sections + 1] = f_system end
        if console_block ~= "" then sections[#sections + 1] = console_block end
        if system_full ~= "" then sections[#sections + 1] = system_full end
        if f_task ~= "" then sections[#sections + 1] = f_task end

        -- === Long context (history can be ~100K tokens) ===
        if history_block ~= "" then sections[#sections + 1] = history_block end

        -- === Bottom anchor: re-ground after long context ===
        -- Repeat f:system, Metrics, Skills/Tools, f:task so the model re-focuses
        -- on its identity, active agents, available tools, and project context.
        -- f:task is included here because project-specific instructions (task description,
        -- constraints) are equally susceptible to "lost in the middle" after ~100K tokens.
        if f_system ~= "" then sections[#sections + 1] = f_system end
        if console_block ~= "" then sections[#sections + 1] = console_block end
        if system_full ~= "" then sections[#sections + 1] = system_full end
        if f_task ~= "" then sections[#sections + 1] = f_task end

        -- === Final: recent operational context + user message ===
        if delegation_block ~= "" then sections[#sections + 1] = delegation_block end
        if f_guard ~= "" then sections[#sections + 1] = f_guard end
        sections[#sections + 1] = message
    end

    return table.concat(sections, "\n\n")
end

-- === Request Handlers ===

--- Handle status: return current agent state for observability.
local function handle_status()
    return {
        success = true,
        data = {
            busy = busy,
            session_id = session_id,
            turn_count = turn_count,
            last_cost = last_cost,
            provider = last_provider,
            model = last_model,
        },
    }
end

--- Handle ping: lightweight LLM provider connectivity check.
local function handle_ping(input)
    local ping_opts = build_llm_opts(input)
    local ping_resp = orcs.llm_ping(ping_opts)
    return {
        success = ping_resp.ok,
        data = ping_resp,
    }
end

--- Handle process: main LLM processing from AgentTask event.
--- Uses pcall to guarantee busy=false even if orcs.llm() throws a Lua error.
---
--- Session management:
---   The concierge stores session_id internally after the first successful LLM call.
---   On subsequent turns, session_id is automatically passed to orcs.llm() to resume
---   the conversation without full prompt reassembly. Callers may also provide
---   input.session_id to override (e.g. external concierge scenarios).
local function handle_process(input)
    local message = input.message or ""
    if message == "" then
        return { success = true }
    end

    -- Reject concurrent processing: only one AgentTask at a time.
    -- Without this guard, two rapid AgentTask events could execute
    -- simultaneous LLM calls, corrupting session state.
    if busy then
        orcs.log("warn", "concierge: rejecting AgentTask — already processing")
        return { success = false, error = "concierge is busy" }
    end

    busy = true
    turn_count = turn_count + 1

    local ok, result = pcall(function()
        -- Session resumption: use stored session_id or caller-provided override.
        -- When session_id is available, skip full prompt assembly and append
        -- the message directly to the existing LLM session.
        local resume_sid = input.session_id or session_id
        if resume_sid and resume_sid ~= "" then
            orcs.log("info", "concierge: resuming session " .. resume_sid:sub(1, 20))
            orcs.output("[Concierge] Thinking...")
            local opts = build_llm_opts(input)
            opts.session_id = resume_sid
            opts.resolve = true
            local llm_resp = orcs.llm(message, opts)
            if llm_resp and llm_resp.ok then
                update_tracking(input, llm_resp)
                emit_success(message, llm_resp)
                return {
                    success = true,
                    data = {
                        response = llm_resp.content,
                        session_id = llm_resp.session_id,
                        num_turns = llm_resp.num_turns,
                        cost = llm_resp.cost,
                        source = "concierge",
                    },
                }
            else
                local err = (llm_resp and llm_resp.error) or "llm resume failed"
                local kind = (llm_resp and llm_resp.error_kind) or "unknown"
                if kind == "overall_timeout" then
                    orcs.log("error", "concierge: overall timeout exceeded during resume (session=" .. tostring(resume_sid) .. ")")
                else
                    orcs.log("warn", "concierge: LLM resume failed (" .. kind .. "): " .. tostring(err))
                end
                orcs.output_with_level("[concierge] Error: " .. tostring(err), "error")
                return { success = false, error = err }
            end
        end

        -- First call: full prompt assembly
        local history_context = input.history_context or ""
        local delegation_context = input.delegation_context or ""
        local placement = input.prompt_placement or "both"

        local f_system, f_task, f_guard = fetch_foundation()
        local console_block = fetch_console_metrics()
        local recommendation, skill_count = fetch_and_register_skills(message, history_context)
        register_delegate_intent()

        -- Compose system block: concierge role + skill recommendations.
        -- Tool definitions are provided exclusively via the API tools parameter
        -- (built from IntentRegistry in build_tools_for_provider). Embedding tool
        -- descriptions in the prompt text would conflict with native tool_use.
        local concierge_role = table.concat({
            "## Your Role: Concierge (Coordinator)",
            "You are the concierge — a coordinating agent, NOT an implementer.",
            "Your job is to understand the user's intent, plan the approach, and delegate work.",
            "",
            "### Capabilities",
            "- Read files, search code (read, grep, glob) for analysis and planning",
            "- Delegate implementation tasks to sub-agents via `delegate_task`",
            "- Invoke skills for specialized workflows",
            "",
            "### Constraints",
            "- You do NOT have write/edit capability. Do not attempt to write or edit files directly.",
            "- For any task that requires creating or modifying files, use `delegate_task` to assign",
            "  the work to a sub-agent that has full tool access (read, write, grep, glob, exec).",
            "- Focus on: understanding requirements, reading relevant code, planning, and delegating.",
            "- Provide concise status updates to the user about delegated work.",
        }, "\n")

        local system_parts = {}
        system_parts[#system_parts + 1] = concierge_role
        if recommendation ~= "" then
            system_parts[#system_parts + 1] = recommendation
        end
        local system_full = table.concat(system_parts, "\n\n")

        -- Compose context blocks
        local history_block = ""
        if history_context ~= "" then
            history_block = "## Recent Conversation\n" .. history_context
        end
        local delegation_block = ""
        if delegation_context ~= "" then
            delegation_block = delegation_context
        end

        -- Assemble prompt
        local prompt = assemble_prompt({
            message = message,
            placement = placement,
            f_system = f_system,
            f_task = f_task,
            f_guard = f_guard,
            console_block = console_block,
            system_full = system_full,
            history_block = history_block,
            delegation_block = delegation_block,
        })

        orcs.log("info", string.format(
            "concierge: prompt built (placement=%s, skills=%d, history=%d, foundation=%d+%d+%d, metrics=%d chars)",
            placement, skill_count, #history_context,
            #f_system, #f_task, #f_guard, #console_block
        ))

        -- Call LLM with tool-use auto-resolution
        orcs.output("[Concierge] Thinking...")
        local llm_opts = build_llm_opts(input)
        llm_opts.resolve = true
        local llm_resp = orcs.llm(prompt, llm_opts)

        if llm_resp and llm_resp.ok then
            update_tracking(input, llm_resp)
            emit_success(message, llm_resp)
            return {
                success = true,
                data = {
                    response = llm_resp.content,
                    session_id = llm_resp.session_id,
                    num_turns = llm_resp.num_turns,
                    cost = llm_resp.cost,
                    source = "concierge",
                },
            }
        else
            local err = (llm_resp and llm_resp.error) or "llm call failed"
            local kind = (llm_resp and llm_resp.error_kind) or "unknown"
            if kind == "overall_timeout" then
                orcs.log("error", "concierge: overall timeout exceeded (" .. tostring(err) .. ")")
            else
                orcs.log("warn", "concierge: LLM call failed (" .. kind .. "): " .. tostring(err))
            end
            orcs.output_with_level("[concierge] Error: " .. tostring(err), "error")
            return {
                success = false,
                error = err,
            }
        end
    end)

    -- Guarantee busy reset regardless of pcall outcome
    busy = false

    if not ok then
        local err_msg = tostring(result)
        orcs.output_with_level("[concierge] Error: " .. err_msg, "error")
        orcs.emit_event("Extension", "llm_response", {
            error = err_msg,
            source = "concierge",
        })
        return { success = false, error = err_msg }
    end

    return result
end

-- === Component Definition ===

return {
    id = "concierge",
    namespace = "builtin",
    subscriptions = {"AgentTask"},

    on_request = function(request)
        local operation = request.operation or "process"

        if operation == "status" then
            return handle_status()
        end

        if operation == "ping" then
            return handle_ping(request.payload or {})
        end

        if operation == "process" then
            return handle_process(request.payload or {})
        end

        return { success = false, error = "unknown operation: " .. tostring(operation) }
    end,

    on_signal = function(signal)
        if signal.kind == "Veto" then
            orcs.log("warn", "concierge: aborted by Veto signal (busy=" .. tostring(busy) .. ")")
            return "Abort"
        end
        return "Handled"
    end,
}

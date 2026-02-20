-- agent_mgr.lua
-- Agent Manager component: routes UserInput to specialized workers.
--
-- Architecture:
--   agent_mgr (Router Component)
--     └── llm-worker (Independent Component, spawned via orcs.spawn_runner())
--         — communicates via orcs.request() RPC
--         — has own Lua VM, event loop, and ChannelRunner
--         — calls Claude Code CLI via orcs.llm()
--
-- @command routing:
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
--   top    — [f:system] [Metrics] [Skills/Tools] [f:task] [History] [f:guard] [UserInput]
--   both   — [f:system] [Metrics] [Skills/Tools] [f:task] [History] [Skills/Tools] [f:guard] [UserInput]  (default)
--   bottom — [f:system] [Metrics] [f:task] [History] [f:guard] [UserInput] [Skills/Tools]

-- === Worker Scripts ===

-- LLM Worker: spawned as an independent Component via orcs.spawn_runner().
-- Has its own Lua VM, event loop, and ChannelRunner. Communicates via RPC.
-- Fetches skill catalog for context, then calls Claude Code CLI.
-- Prompt assembly respects `prompt_placement` strategy passed from parent.
local llm_worker_script = [[
return {
    id = "llm-worker",
    namespace = "builtin",
    subscriptions = {},  -- RPC-only: no event subscriptions needed

    -- Independent Component: receives work via on_request (RPC).
    -- operation="process" is the primary entrypoint from agent_mgr.
    on_request = function(request)
        local input = request.payload or {}
        local message = input.message or ""

        -- Build LLM opts from config passed via RPC payload
        local function build_llm_opts(input)
            local opts = {}
            local llm_cfg = input.llm_config or {}
            if llm_cfg.provider   then opts.provider    = llm_cfg.provider end
            if llm_cfg.model      then opts.model       = llm_cfg.model end
            if llm_cfg.base_url   then opts.base_url    = llm_cfg.base_url end
            if llm_cfg.api_key    then opts.api_key     = llm_cfg.api_key end
            if llm_cfg.temperature then opts.temperature = llm_cfg.temperature end
            if llm_cfg.max_tokens then opts.max_tokens  = llm_cfg.max_tokens end
            if llm_cfg.timeout    then opts.timeout     = llm_cfg.timeout end
            return opts
        end

        -- Session resumption: skip full prompt assembly, send message directly
        if input.session_id and input.session_id ~= "" then
            orcs.log("debug", "llm-worker: resuming session " .. input.session_id:sub(1, 20))
            local opts = build_llm_opts(input)
            opts.session_id = input.session_id
            local llm_resp = orcs.llm(message, opts)
            if llm_resp and llm_resp.ok then
                return {
                    success = true,
                    data = {
                        response = llm_resp.content,
                        session_id = llm_resp.session_id,
                        num_turns = llm_resp.num_turns,
                        cost = llm_resp.cost,
                        source = "llm-worker",
                    },
                }
            else
                local err = (llm_resp and llm_resp.error) or "llm resume failed"
                return { success = false, error = err }
            end
        end

        -- First call: full prompt assembly
        local history_context = input.history_context or ""
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
            orcs.log("debug", "llm-worker: foundation segments unavailable, proceeding without")
        end

        -- 0b. Fetch console metrics (git, cwd, timestamp)
        local console_block = ""
        local m_resp = orcs.request("metrics::console_metrics", "get_all", {})
        if m_resp and m_resp.success and m_resp.formatted then
            console_block = m_resp.formatted
        else
            orcs.log("debug", "llm-worker: console metrics unavailable, proceeding without")
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
        -- @command reference (passed from parent via RPC payload)
        local cmd_ref = input.command_reference or ""
        if cmd_ref ~= "" then
            system_parts[#system_parts + 1] = cmd_ref
        end
        local system_full = table.concat(system_parts, "\n\n")

        -- 3. Compose history block
        local history_block = ""
        if history_context ~= "" then
            history_block = "## Recent Conversation\n" .. history_context
        end

        -- 4. Assemble prompt based on placement strategy
        --
        -- Fixed positions:
        --   f_system      → always at top (identity/core rules)
        --   console_block → after f_system (workspace context)
        --   f_task        → before history (project context)
        --   f_guard       → just before user message (output constraints)
        --
        -- Placement strategy controls skill/tool block (system_full) position only.
        local sections = {}

        -- Foundation:system — always top
        if f_system ~= "" then sections[#sections + 1] = f_system end

        -- Console metrics — always after f_system
        if console_block ~= "" then sections[#sections + 1] = console_block end

        if placement == "top" then
            -- [f:system] [Metrics] [Skills/Tools] [f:task] [History] [f:guard] [UserInput]
            if system_full ~= "" then sections[#sections + 1] = system_full end
            if f_task ~= "" then sections[#sections + 1] = f_task end
            if history_block ~= "" then sections[#sections + 1] = history_block end
            if f_guard ~= "" then sections[#sections + 1] = f_guard end
            sections[#sections + 1] = message

        elseif placement == "bottom" then
            -- [f:system] [Metrics] [f:task] [History] [f:guard] [UserInput] [Skills/Tools]
            if f_task ~= "" then sections[#sections + 1] = f_task end
            if history_block ~= "" then sections[#sections + 1] = history_block end
            if f_guard ~= "" then sections[#sections + 1] = f_guard end
            sections[#sections + 1] = message
            if system_full ~= "" then sections[#sections + 1] = system_full end

        else
            -- "both" (default):
            -- [f:system] [Metrics] [Skills/Tools] [f:task] [History] [Skills/Tools] [f:guard] [UserInput]
            if system_full ~= "" then sections[#sections + 1] = system_full end
            if f_task ~= "" then sections[#sections + 1] = f_task end
            if history_block ~= "" then sections[#sections + 1] = history_block end
            if system_full ~= "" then sections[#sections + 1] = system_full end
            if f_guard ~= "" then sections[#sections + 1] = f_guard end
            sections[#sections + 1] = message
        end

        local prompt = table.concat(sections, "\n\n")

        orcs.log("debug", string.format(
            "llm-worker: prompt built (placement=%s, skills=%d, history=%d, tools=%d, cmds=%d, foundation=%d+%d+%d, metrics=%d chars)",
            placement, skill_count, #history_context, #tool_desc, #cmd_ref,
            #f_system, #f_task, #f_guard, #console_block
        ))

        -- 5. Call LLM via orcs.llm() with provider/model from config
        -- Note: session_id is handled by the early-return path above;
        -- this path is always a fresh first call.
        local llm_opts = build_llm_opts(input)
        local llm_resp = orcs.llm(prompt, llm_opts)
        if llm_resp and llm_resp.ok then
            return {
                success = true,
                data = {
                    response = llm_resp.content,
                    session_id = llm_resp.session_id,
                    num_turns = llm_resp.num_turns,
                    cost = llm_resp.cost,
                    source = "llm-worker",
                },
            }
        else
            local err = (llm_resp and llm_resp.error) or "llm call failed"
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

-- === Constants & Settings ===

local HISTORY_LIMIT = 10  -- Max recent conversation entries to include as context
local MAX_REACT_ITERATIONS = 5  -- Max tool-use loop iterations before returning

-- Component settings (populated from config in init())
local component_settings = {}

-- FQN of the spawned LLM worker Component (set in init()).
-- Used by dispatch_llm() to send RPC requests.
local llm_worker_fqn = nil

-- Valid prompt placement strategies
local VALID_PLACEMENTS = { top = true, both = true, bottom = true }

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

--- Build @command reference text for LLM instructions.
local function build_command_reference()
    local descriptions = {
        shell      = "Execute shell command (ls, git, cargo, etc.)",
        tool       = "File operations: read, write, grep, glob, mkdir, remove, mv, pwd",
        skill      = "Skill management: list, get <name>, activate <name>, deactivate <name>, status",
        profile    = "Profile switching: list, current, use <name>",
        foundation = "Foundation segments: get_all, status",
    }
    local lines = {
        "## @Command Reference",
        "To execute actions, write a line starting with `@<prefix>`:",
        "",
    }
    for _, prefix in ipairs({"shell", "tool", "skill", "profile", "foundation"}) do
        if routes[prefix] then
            lines[#lines + 1] = string.format("  @%-12s — %s", prefix, descriptions[prefix] or "(available)")
        end
    end
    lines[#lines + 1] = ""
    lines[#lines + 1] = "Each @command MUST be on its own line, NOT inside ``` code blocks."
    lines[#lines + 1] = "When your task is complete, respond normally without @commands."
    return table.concat(lines, "\n")
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

--- Extract @commands from LLM response text.
--- Only matches lines starting with a registered route prefix to avoid false positives.
--- Lines inside fenced code blocks (```) are skipped to prevent accidental execution
--- of example commands in LLM output.
--- Returns a list of {prefix=..., body=...} tables.
local function extract_commands(text)
    local commands = {}
    local in_code_block = false
    for line in text:gmatch("[^\n]+") do
        local trimmed = line:match("^%s*(.-)%s*$")
        -- Toggle code block state on fence lines (``` with optional language tag)
        if trimmed:match("^```") then
            in_code_block = not in_code_block
        elseif not in_code_block then
            local prefix, body = trimmed:match("^@(%w+)%s+(.+)$")
            if not prefix then
                prefix = trimmed:match("^@(%w+)%s*$")
                body = ""
            end
            if prefix then
                local p = prefix:lower()
                if routes[p] then
                    commands[#commands + 1] = { prefix = p, body = body or "" }
                end
            end
        end
    end
    return commands
end

--- Format command execution results into text for LLM continuation.
local function format_command_results(results)
    local parts = {}
    for _, r in ipairs(results) do
        local status = r.success and "OK" or "ERROR"
        local content = r.response or r.error or "(no output)"
        if type(content) == "table" then
            content = orcs.json_encode(content)
        end
        parts[#parts + 1] = string.format("[@%s result: %s]\n%s", r.prefix, status, tostring(content))
    end
    return table.concat(parts, "\n\n")
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

--- Default route: send to llm-worker via RPC with ReAct loop.
--- On each iteration, the LLM response is scanned for @-prefixed commands
--- directed at registered routes. Commands are executed and results fed back
--- via --resume (session continuity) until the LLM produces a final answer
--- (no more commands) or the iteration limit is reached.
local function dispatch_llm(message)
    -- Guard: llm-worker must be initialized
    if not llm_worker_fqn then
        orcs.output_with_level("[AgentMgr] Error: LLM worker not initialized", "error")
        return { success = false, error = "llm worker not initialized" }
    end

    -- Gather history in parent context (emitter function, unavailable to children)
    local history_context = fetch_history_context()

    -- Resolve prompt placement strategy from config
    local placement = component_settings.prompt_placement or "both"
    if not VALID_PLACEMENTS[placement] then
        orcs.log("warn", "Invalid prompt_placement '" .. placement .. "', falling back to both")
        placement = "both"
    end

    -- Build @command reference for LLM instructions
    local command_reference = build_command_reference()

    -- Extract LLM-specific config (llm_* keys → config table without prefix)
    local llm_config = {}
    local llm_key_map = {
        llm_provider    = "provider",
        llm_model       = "model",
        llm_base_url    = "base_url",
        llm_api_key     = "api_key",
        llm_temperature = "temperature",
        llm_max_tokens  = "max_tokens",
        llm_timeout     = "timeout",
    }
    for src_key, dst_key in pairs(llm_key_map) do
        if component_settings[src_key] ~= nil then
            llm_config[dst_key] = component_settings[src_key]
        end
    end

    local session_id = nil
    local last_response = nil
    local last_response_shown = false
    local total_cost = 0
    local current_message = message

    for iteration = 1, MAX_REACT_ITERATIONS do
        -- Build RPC payload for llm-worker
        local payload = {
            message = current_message,
            prompt_placement = placement,
            llm_config = llm_config,
        }
        if session_id then
            -- Continuation: llm-worker will skip prompt assembly and just --resume
            payload.session_id = session_id
        else
            -- First call: include history and @command reference for full prompt assembly
            payload.history_context = history_context
            payload.command_reference = command_reference
        end

        -- RPC to llm-worker Component (independent ChannelRunner)
        local result = orcs.request(llm_worker_fqn, "process", payload)
        if not result or not result.success then
            local err = (result and result.error) or "unknown"
            orcs.output_with_level("[AgentMgr] Error: " .. err, "error")
            return { success = false, error = err }
        end

        local data = result.data or {}
        local response = data.response or "no response"
        session_id = data.session_id
        total_cost = total_cost + (data.cost or 0)
        last_response = response
        last_response_shown = false

        -- Scan for @commands targeting registered routes
        local commands = extract_commands(response)
        if #commands == 0 then
            -- Final answer: no more commands to execute
            break
        end

        -- Show intermediate LLM response (user sees the reasoning)
        orcs.output(response)
        last_response_shown = true

        -- Guard: cannot continue without session_id
        if not session_id then
            orcs.log("warn", "ReAct: no session_id returned, cannot resume — stopping loop")
            break
        end

        -- Guard: reached iteration limit
        if iteration == MAX_REACT_ITERATIONS then
            orcs.log("warn", "ReAct: reached max iterations (" .. MAX_REACT_ITERATIONS .. "), stopping")
            break
        end

        orcs.log("info", string.format(
            "ReAct iteration %d/%d: executing %d command(s)",
            iteration, MAX_REACT_ITERATIONS, #commands
        ))

        -- Execute all extracted commands
        local cmd_results = {}
        for _, cmd in ipairs(commands) do
            orcs.log("info", string.format("ReAct: @%s %s", cmd.prefix, utf8_truncate(cmd.body, 80)))
            local route = routes[cmd.prefix]
            local cmd_result = dispatch_route(route, cmd.body)
            cmd_results[#cmd_results + 1] = {
                prefix = cmd.prefix,
                success = cmd_result.success,
                response = cmd_result.data and cmd_result.data.response,
                error = cmd_result.error,
            }
        end

        -- Prepare continuation message with command results
        current_message = format_command_results(cmd_results)
    end

    -- Output final LLM response (skip if already shown as intermediate)
    if not last_response_shown then
        orcs.output(last_response)
    end
    orcs.emit_event("Extension", "llm_response", {
        message = message,
        response = last_response,
        source = "llm-worker",
        cost = total_cost,
        session_id = session_id,
    })
    orcs.log("debug", string.format(
        "dispatch_llm: completed (cost=%.4f, session=%s)",
        total_cost or 0, session_id or "none"
    ))
    return {
        success = true,
        data = {
            message = message,
            response = last_response,
            source = "llm-worker",
            cost = total_cost,
            session_id = session_id,
        },
    }
end

-- === Component Definition ===

return {
    id = "agent_mgr",
    namespace = "builtin",
    subscriptions = {"UserInput"},
    output_to_io = true,
    elevated = true,
    child_spawner = true,

    on_request = function(request)
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
                -- Unknown @prefix: pass full message to llm-worker as-is
                orcs.log("debug", "AgentMgr: unknown prefix @" .. prefix .. ", falling through to llm")
            end
        end

        -- Default: route to llm-worker
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

        local placement = component_settings.prompt_placement or "both"
        local llm_provider = component_settings.llm_provider or "(default)"
        local llm_model = component_settings.llm_model or "(default)"
        orcs.log("info", string.format(
            "agent_mgr initializing (prompt_placement=%s, llm=%s/%s)...",
            placement, llm_provider, llm_model
        ))

        -- Spawn LLM worker as an independent Component via spawn_runner().
        -- Rust-side spawn_runner_from_script() uses block_in_place to synchronously
        -- wait for World registration (ready notification via oneshot channel).
        -- When this call returns, the Component is fully registered and RPC-reachable.
        local llm = orcs.spawn_runner({
            script = llm_worker_script,
        })
        if llm.ok then
            llm_worker_fqn = llm.fqn
            orcs.log("info", "spawned llm-worker as Component (fqn=" .. llm_worker_fqn .. ")")
        else
            orcs.log("error", "failed to spawn llm-worker: " .. (llm.error or ""))
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

        orcs.output("[AgentMgr] Ready (worker: " .. (llm_worker_fqn or "failed") .. ")")
        orcs.log("info", "agent_mgr initialized")
    end,

    shutdown = function()
        orcs.log("info", "agent_mgr shutdown")
    end,
}

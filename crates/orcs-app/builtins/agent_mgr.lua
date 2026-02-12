-- agent_mgr.lua
-- Agent Manager component: routes UserInput to specialized workers.
--
-- Architecture:
--   agent_mgr (Router)
--     ├── llm-worker    — default handler, calls Claude Code CLI via orcs.llm()
--     └── skill-worker  — skill queries via RPC to skill_manager

-- === Worker Scripts ===

-- LLM Worker: fetches skill catalog for context, then calls Claude Code CLI.
local llm_worker_script = [[
return {
    id = "llm-worker",

    run = function(input)
        local message = input.message or ""

        -- 1. Select relevant skills for this message
        local skill_context = ""
        local skill_count = 0
        local select_resp = orcs.request("skill::skill_manager", "select", {
            query = message,
            limit = 5,
        })
        if select_resp and select_resp.success and select_resp.data then
            local skills = select_resp.data
            if type(skills) == "table" and #skills > 0 then
                skill_count = #skills
                local lines = { "\n\n## Relevant Skills" }
                for _, s in ipairs(skills) do
                    local line = "- **" .. (s.name or "?") .. "**"
                    if s.description then
                        line = line .. ": " .. s.description
                    end
                    lines[#lines + 1] = line
                end
                skill_context = table.concat(lines, "\n")
            end
        end

        -- 2. Gather tool descriptions for the Agent CLI
        local tool_desc = ""
        if orcs.tool_descriptions then
            local td = orcs.tool_descriptions()
            if td and td ~= "" then
                tool_desc = "\n\n## Available ORCS Tools\n" .. td
            end
        end

        -- 3. Use conversation history passed from parent (agent_mgr)
        -- board_recent is an emitter function unavailable to child workers,
        -- so the parent fetches it and passes via input.history_context.
        local history_context = input.history_context or ""

        -- 4. Build prompt with context
        local prompt = message
        if skill_context ~= "" or tool_desc ~= "" or history_context ~= "" then
            prompt = message .. history_context .. skill_context .. tool_desc
            orcs.log("debug", "llm-worker: context added (history=" .. #history_context .. ", skills=" .. skill_count .. ", tools=" .. #tool_desc .. " chars)")
        else
            orcs.log("debug", "llm-worker: no additional context")
        end

        -- 5. Call Claude Code CLI (headless)
        local llm_resp = orcs.llm(prompt)
        if llm_resp and llm_resp.ok then
            return {
                success = true,
                data = {
                    response = llm_resp.content,
                    source = "llm-worker",
                },
            }
        else
            local err = (llm_resp and llm_resp.error) or "llm call failed"
            return {
                success = false,
                error = err,
                data = { source = "llm-worker" },
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

-- Skill Worker: lightweight skill queries via RPC.
local skill_worker_script = [[
return {
    id = "skill-worker",

    run = function(input)
        local operation = input.operation or "status"
        local payload = input.payload or {}

        local resp = orcs.request("skill::skill_manager", operation, payload)
        if resp and resp.success then
            return {
                success = true,
                data = {
                    result = resp.data,
                    source = "skill-worker",
                },
            }
        else
            local err = (resp and resp.error) or "skill request failed"
            return {
                success = false,
                error = err,
                data = { source = "skill-worker" },
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

-- === Constants ===

local HISTORY_LIMIT = 10  -- Max recent conversation entries to include as context

-- === Routing ===

--- Route table: prefix → { target_type, target }
--- target_type is "rpc" (orcs.request to a component) or "child" (send_to_child).
local routes = {
    shell   = { type = "rpc",   target = "builtin::shell" },
    skill   = { type = "child", target = "skill-worker" },
    profile = { type = "rpc",   target = "profile::profile_manager" },
    tool    = { type = "rpc",   target = "builtin::tool" },
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

--- Execute an RPC route.
local function dispatch_rpc(route, body)
    local resp = orcs.request(route.target, "input", { message = body })
    if resp and resp.success then
        orcs.emit_event("Extension", "route_response", {
            route_type = "rpc",
            target = route.target,
            message = body,
        })
        orcs.log("debug", "dispatch_rpc: emitted route_response event for " .. route.target)
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

--- Execute a child-worker route.
local function dispatch_child(route, body)
    local result = orcs.send_to_child(route.target, { message = body, operation = "input", payload = { message = body } })
    if result and result.ok then
        local data = result.result or {}
        local response = data.response or data.result
        if type(response) == "table" then
            response = orcs.json_encode(response)
        end
        orcs.emit_event("Extension", "route_response", {
            route_type = "child",
            target = route.target,
            message = body,
        })
        orcs.log("debug", "dispatch_child: emitted route_response event for " .. route.target)
        return {
            success = true,
            data = {
                response = response or "ok",
                source = route.target,
            },
        }
    else
        local err = (result and result.error) or "unknown"
        orcs.output("[AgentMgr] @" .. route.target .. " error: " .. err, "error")
        return { success = false, error = err }
    end
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
    local lines = { "\n\n## Recent Conversation" }
    for _, entry in ipairs(entries) do
        local payload = entry.payload or {}
        local text = payload.message or payload.content or payload.response
        if text and type(text) == "string" and text ~= "" then
            local src = entry.source or "unknown"
            lines[#lines + 1] = string.format("- [%s] %s", src, text:sub(1, 200))
        end
    end
    if #lines > 1 then
        return table.concat(lines, "\n")
    end
    return ""
end

--- Default route: send to llm-worker.
local function dispatch_llm(message)
    -- Gather history in parent context (emitter function, unavailable to children)
    local history_context = fetch_history_context()

    local result = orcs.send_to_child("llm-worker", {
        message = message,
        history_context = history_context,
    })
    if result and result.ok then
        local data = result.result or {}
        local response = data.response or "no response"
        orcs.output(response)
        orcs.emit_event("Extension", "llm_response", {
            message = message,
            response = response,
            source = data.source or "llm-worker",
        })
        orcs.log("debug", "dispatch_llm: emitted llm_response event")
        return {
            success = true,
            data = {
                message = message,
                response = response,
                source = data.source or "llm-worker",
            },
        }
    else
        local err = (result and result.error) or "unknown"
        orcs.output("[AgentMgr] Error: " .. err, "error")
        return { success = false, error = err }
    end
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

        orcs.log("info", "AgentMgr received: " .. message:sub(1, 50))

        -- Parse @prefix routing
        local prefix, body = parse_route(message)

        if prefix then
            local route = routes[prefix]
            if route then
                orcs.log("info", "AgentMgr routing @" .. prefix .. " -> " .. route.target)
                if route.type == "rpc" then
                    return dispatch_rpc(route, body)
                else
                    return dispatch_child(route, body)
                end
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

    init = function()
        orcs.log("info", "agent_mgr initializing...")

        -- Spawn LLM worker (default handler)
        local llm = orcs.spawn_child({
            id = "llm-worker",
            script = llm_worker_script,
        })
        if llm.ok then
            orcs.log("info", "spawned llm-worker")
        else
            orcs.log("error", "failed to spawn llm-worker: " .. (llm.error or ""))
        end

        -- Spawn skill worker (internal service)
        local skill = orcs.spawn_child({
            id = "skill-worker",
            script = skill_worker_script,
        })
        if skill.ok then
            orcs.log("info", "spawned skill-worker")
        else
            orcs.log("error", "failed to spawn skill-worker: " .. (skill.error or ""))
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

        orcs.output("[AgentMgr] Ready (workers: llm, skill)")
        orcs.log("info", "agent_mgr initialized")
    end,

    shutdown = function()
        orcs.log("info", "agent_mgr shutdown")
    end,
}

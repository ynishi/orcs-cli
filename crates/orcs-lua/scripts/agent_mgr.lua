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

        -- 1. Fetch skill catalog for context enrichment
        local skill_context = ""
        local catalog_resp = orcs.request("skill::skill_manager", "catalog", {})
        if catalog_resp and catalog_resp.success and catalog_resp.data then
            local catalog = catalog_resp.data.catalog
            if catalog and catalog ~= "" then
                skill_context = "\n\n## Available Skills\n" .. catalog
            end
        end

        -- 2. Build prompt with context
        local prompt = message
        if skill_context ~= "" then
            prompt = message .. skill_context
            orcs.log("debug", "llm-worker: skill_context attached (" .. #skill_context .. " chars)")
        else
            orcs.log("debug", "llm-worker: no skill_context")
        end

        -- 3. Call Claude Code CLI (headless)
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

        -- Route to llm-worker (default)
        local result = orcs.send_to_child("llm-worker", { message = message })
        if result.ok then
            local data = result.result or {}
            local response = data.response or "no response"
            orcs.output(response)
            return {
                success = true,
                data = {
                    message = message,
                    response = response,
                    source = data.source,
                },
            }
        else
            local err = result.error or "unknown"
            orcs.output("[AgentMgr] Error: " .. err, "error")
            return { success = false, error = err }
        end
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

        orcs.output("[AgentMgr] Ready (workers: llm, skill)")
        orcs.log("info", "agent_mgr initialized")
    end,

    shutdown = function()
        orcs.log("info", "agent_mgr shutdown")
    end,
}

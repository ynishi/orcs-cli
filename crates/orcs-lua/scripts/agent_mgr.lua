-- agent_mgr.lua
-- Agent Manager component that spawns and manages child agents.
--
-- Usage: Spawns a child agent on init and delegates UserInput to children.

-- Child agent script (embedded)
local child_script = [[
return {
    id = "worker",
    namespace = "child",
    subscriptions = {},

    on_request = function(request)
        local message = request.payload
        if type(message) == "table" then
            message = message.message or ""
        end
        return {
            success = true,
            data = {
                response = "Worker processed: " .. tostring(message),
                source = "worker"
            }
        }
    end,

    on_signal = function(signal)
        if signal.kind == "Veto" then
            return "Abort"
        end
        return "Ignored"
    end,
}
]]

local worker_handle = nil

return {
    id = "agent_mgr",
    namespace = "builtin",
    subscriptions = {"UserInput"},

    on_request = function(request)
        -- Extract user message from payload
        local message = request.payload
        if type(message) == "table" then
            message = message.message or message.content or ""
        end
        if type(message) ~= "string" or message == "" then
            return { success = false, error = "empty message" }
        end

        orcs.log("info", "AgentMgr received: " .. message:sub(1, 50))

        -- Check if worker is spawned
        local child_count = orcs.child_count()
        if child_count > 0 then
            orcs.output("[AgentMgr] Delegating to " .. child_count .. " worker(s): " .. message)
        else
            orcs.output("[AgentMgr] No workers available, message: " .. message)
        end

        return {
            success = true,
            data = {
                message = message,
                workers = child_count
            }
        }
    end,

    on_signal = function(signal)
        if signal.kind == "Veto" then
            return "Abort"
        end
        return "Ignored"
    end,

    init = function()
        orcs.log("info", "agent_mgr component initializing...")

        -- Try to spawn a child worker
        local result = orcs.spawn_child({
            id = "worker-1",
            script = child_script
        })

        if result.ok then
            worker_handle = result
            orcs.log("info", "agent_mgr spawned worker: " .. result.id)
            orcs.output("[AgentMgr] Worker spawned: " .. result.id)
        else
            orcs.log("warn", "agent_mgr failed to spawn worker: " .. (result.error or "unknown"))
            orcs.output("[AgentMgr] Failed to spawn worker: " .. (result.error or "unknown"))
        end

        orcs.log("info", "agent_mgr component initialized")
    end,

    shutdown = function()
        orcs.log("info", "agent_mgr component shutdown")
    end,
}

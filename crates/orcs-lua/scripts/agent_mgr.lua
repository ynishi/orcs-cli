-- agent_mgr.lua
-- Agent Manager component that spawns and manages child agents.
--
-- Usage: Spawns a child agent on init and delegates UserInput to children.

-- Child worker script (embedded)
-- Note: Workers use `run` function (not `on_request`) because they are Children, not Components.
local child_script = [[
return {
    id = "worker",

    run = function(input)
        local message = input.message or ""
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
        return "Handled"
    end,
}
]]

local worker_handle = nil

return {
    id = "agent_mgr",
    namespace = "builtin",
    subscriptions = {"UserInput"},
    output_to_io = true,
    elevated = true,
    child_spawner = true,

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
            -- Delegate to worker
            local result = orcs.send_to_child("worker-1", { message = message })
            if result.ok then
                local response = result.result and result.result.response or "no response"
                orcs.output("[AgentMgr] Worker response: " .. tostring(response))
                return {
                    success = true,
                    data = {
                        message = message,
                        worker_response = response
                    }
                }
            else
                orcs.output("[AgentMgr] Worker error: " .. (result.error or "unknown"))
                return { success = false, error = result.error }
            end
        else
            orcs.output("[AgentMgr] No workers available, message: " .. message)
            return {
                success = true,
                data = {
                    message = message,
                    workers = 0
                }
            }
        end
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

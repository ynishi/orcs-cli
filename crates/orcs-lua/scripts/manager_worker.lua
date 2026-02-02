-- manager_worker.lua
-- Manager component that spawns and manages worker children.
--
-- This script demonstrates the Mgr-Sub pattern where a Component (Manager)
-- spawns and controls Child entities (Workers).
--
-- Component can spawn children via set_child_context() (see tests).

return {
    id = "manager",
    namespace = "test",
    subscriptions = {"Echo"},

    -- State for tracking workers
    _state = {
        workers = {},
        task_count = 0,
    },

    init = function()
        orcs.log("info", "Manager initialized")
    end,

    on_request = function(req)
        if req.operation == "dispatch" then
            -- For now, directly process (Component cannot spawn children yet)
            local task = req.payload.task or "unknown"
            orcs.output("Processing task: " .. task)
            return {
                success = true,
                data = {
                    processed = task,
                    by = "manager-direct",
                }
            }
        elseif req.operation == "status" then
            return {
                success = true,
                data = {
                    worker_count = 0, -- TODO: when Component can spawn
                    tasks_processed = 0,
                }
            }
        end

        return { success = false, error = "unknown operation: " .. req.operation }
    end,

    on_signal = function(sig)
        if sig.kind == "Veto" then
            orcs.output_with_level("Manager received veto, aborting workers...", "warn")
            return "Abort"
        end
        return "Handled"
    end,

    shutdown = function()
        orcs.log("info", "Manager shutting down")
    end,
}

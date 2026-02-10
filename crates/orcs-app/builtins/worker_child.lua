-- worker_child.lua
-- Worker child that can be spawned by a parent and executes tasks.
--
-- This is a RunnableChild that:
-- - Has a run() function for task execution
-- - Responds to signals (Veto -> Abort)
-- - Can spawn sub-children via orcs.spawn_child()
-- - Emits output via orcs.emit_output()

return {
    id = "worker",

    -- run(input) -> { success, data?, error? }
    -- Main execution function for the worker
    run = function(input)
        local task = input.task or "default-task"
        local iterations = input.iterations or 1

        -- Emit progress
        orcs.emit_output("Worker starting task: " .. task)

        -- Simulate work
        local results = {}
        for i = 1, iterations do
            table.insert(results, {
                iteration = i,
                task = task,
                result = task .. "-result-" .. i,
            })
        end

        orcs.emit_output("Worker completed " .. iterations .. " iterations")

        return {
            success = true,
            data = {
                task = task,
                iterations = iterations,
                results = results,
            }
        }
    end,

    on_signal = function(sig)
        if sig.kind == "Veto" then
            -- Now we can use emit_output in on_signal (context is registered)
            if orcs and orcs.emit_output then
                orcs.emit_output("Worker received veto, aborting...", "warn")
            elseif orcs and orcs.log then
                orcs.log("warn", "Worker received veto, aborting...")
            end
            return "Abort"
        elseif sig.kind == "Cancel" then
            return "Handled"
        end
        return "Ignored"
    end,

    abort = function()
        orcs.emit_output("Worker abort() called", "warn")
    end,
}

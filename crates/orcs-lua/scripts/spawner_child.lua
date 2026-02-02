-- spawner_child.lua
-- Child that spawns sub-children to demonstrate nested spawning.
--
-- This demonstrates:
-- - A child spawning other children via orcs.spawn_child()
-- - Querying child_count and max_children
-- - Coordinating work across children

return {
    id = "spawner",

    run = function(input)
        local num_workers = input.num_workers or 2
        local task = input.task or "parallel-task"

        -- Check limits
        local max = orcs.max_children()
        if num_workers > max then
            return {
                success = false,
                error = "requested " .. num_workers .. " workers, max is " .. max,
            }
        end

        -- Spawn workers
        local spawned = {}
        for i = 1, num_workers do
            local worker_id = "sub-worker-" .. i
            local result = orcs.spawn_child({
                id = worker_id,
                script = [[
                    return {
                        id = "]] .. worker_id .. [[",
                        run = function(input)
                            return { success = true, data = { processed = input.task } }
                        end,
                        on_signal = function(sig)
                            return "Handled"
                        end,
                    }
                ]]
            })

            if result.ok then
                table.insert(spawned, result.id)
                orcs.emit_output("Spawned worker: " .. result.id)
            else
                orcs.emit_output("Failed to spawn: " .. (result.error or "unknown"), "error")
            end
        end

        local current_count = orcs.child_count()
        orcs.emit_output("Total children: " .. current_count .. "/" .. max)

        return {
            success = true,
            data = {
                spawned = spawned,
                count = current_count,
                max = max,
            }
        }
    end,

    on_signal = function(sig)
        if sig.kind == "Veto" then
            return "Abort"
        end
        return "Handled"
    end,
}

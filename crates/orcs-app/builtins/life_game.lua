-- life_game.lua
-- 1D Cellular Automaton on a ring topology with colored cells.
--
-- Architecture:
--   life_game (Manager)
--     cell-1 ... cell-N   (worker children, parallel via send_to_children_batch)
--
-- Each cell is a stateless pure function: receives (me, left, right) -> new state.
-- All state lives in the manager; workers compute the next state in parallel.
--
-- Usage:
--   orcs.request("demo::life_game", "run", {
--     grid_size  = 16,       -- number of cells (default 16)
--     generations = 5,       -- simulation steps (default 3)
--   })
--
-- Rules (1D ring, 2 neighbors per cell):
--   Birth:    dead cell with >= 1 alive neighbor -> born (inherits left neighbor's kind)
--   Survival: alive cell with >= 1 alive neighbor -> survives
--   Death:    alive cell with 0 alive neighbors -> dies
--   Conflict: both neighbors alive with different kind -> cell mutates to the other kind

-- === Constants ===

local KINDS = { "R", "G", "B" }
local MAX_CELLS = 64

-- === Cell Worker Script ===
-- Pure function: (me, left, right) -> new state.

local function make_cell_script(cell_id)
    return string.format([[
return {
    id = "%s",
    run = function(input)
        local me    = input.me
        local left  = input.left
        local right = input.right

        local alive = me.alive
        local kind  = me.kind

        local n = 0
        if left.alive  then n = n + 1 end
        if right.alive then n = n + 1 end

        if alive then
            if n == 0 then
                alive = false
            end
            if n == 2 and left.kind ~= right.kind then
                if kind == left.kind then
                    kind = right.kind
                elseif kind == right.kind then
                    kind = left.kind
                end
            end
        else
            if n >= 1 then
                alive = true
                if left.alive then
                    kind = left.kind
                else
                    kind = right.kind
                end
            end
        end

        return {
            success = true,
            data = { alive = alive, kind = kind },
        }
    end,
    on_signal = function(sig)
        if sig.kind == "Veto" then return "Abort" end
        return "Handled"
    end,
}
]], cell_id)
end

-- === Rendering ===

local function render_grid(grid)
    local parts = {}
    for _, cell in ipairs(grid) do
        if cell.alive then
            parts[#parts + 1] = cell.kind
        else
            parts[#parts + 1] = "."
        end
    end
    return table.concat(parts)
end

-- === Default Initial State ===
-- Seed: every 3rd cell dead, kinds cycle R/G/B.

local function default_initial(size)
    local grid = {}
    for i = 1, size do
        local alive = (i % 3 ~= 0)
        local kind_idx = ((i - 1) % #KINDS) + 1
        grid[i] = { alive = alive, kind = KINDS[kind_idx] }
    end
    return grid
end

-- === Simulation State (module-scope, persists across requests) ===

local _grid = {}
local _cell_ids = {}
local _spawned_size = 0

-- === Internal: ensure cell workers are spawned ===

local function ensure_cells(size)
    if _spawned_size >= size then
        return true
    end

    for i = _spawned_size + 1, size do
        local cid = "cell-" .. i
        local result = orcs.spawn_child({
            id = cid,
            script = make_cell_script(cid),
        })
        if result.ok then
            _cell_ids[i] = cid
        else
            orcs.output("[LifeGame] Failed to spawn " .. cid .. ": " .. (result.error or ""))
            return false
        end
    end

    _spawned_size = size
    return true
end

-- === Internal: run one generation ===

local function step(grid, size)
    local ids = {}
    local inputs = {}
    for i = 1, size do
        local left_idx  = (i == 1) and size or (i - 1)
        local right_idx = (i == size) and 1 or (i + 1)
        ids[i] = _cell_ids[i]
        inputs[i] = {
            me    = grid[i],
            left  = grid[left_idx],
            right = grid[right_idx],
        }
    end

    local results = orcs.send_to_children_batch(ids, inputs)

    local new_grid = {}
    for i, r in ipairs(results) do
        if r.ok and r.result then
            new_grid[i] = r.result
        else
            new_grid[i] = grid[i]
            orcs.log("warn", "cell-" .. i .. " error: " .. (r.error or "unknown"))
        end
    end
    return new_grid
end

-- === Internal: count statistics ===

local function stats(grid)
    local alive = 0
    local kinds = {}
    for _, cell in ipairs(grid) do
        if cell.alive then
            alive = alive + 1
            kinds[cell.kind] = (kinds[cell.kind] or 0) + 1
        end
    end
    return alive, kinds
end

-- === Component Definition ===

return {
    id = "life_game",
    namespace = "demo",
    subscriptions = {},
    child_spawner = true,
    output_to_io = true,
    elevated = true,

    on_request = function(request)
        local payload = request.payload or {}
        local op = request.operation or "run"

        -- @life_game <text> sends operation="input", payload={message=...}
        if op == "input" then
            local msg = (payload.message or ""):match("^%s*(.-)%s*$")
            if msg == "" or msg == "run" then
                op = "run"
                payload = {}
            elseif msg == "status" then
                op = "status"
                payload = {}
            else
                -- Try to parse "run 32 10" → grid_size=32, generations=10
                local cmd, arg1, arg2 = msg:match("^(%S+)%s*(%d*)%s*(%d*)$")
                if cmd == "run" then
                    op = "run"
                    payload = {
                        grid_size   = tonumber(arg1) or 16,
                        generations = tonumber(arg2) or 3,
                    }
                else
                    orcs.output("[LifeGame] Usage: run [grid_size] [generations] | status")
                    return { success = true }
                end
            end
        end

        if op == "run" then
            local size = payload.grid_size or 16
            local gens = payload.generations or 3

            if size > MAX_CELLS then
                return { success = false, error = "grid_size max is " .. MAX_CELLS }
            end

            if not ensure_cells(size) then
                return { success = false, error = "failed to spawn cells" }
            end

            _grid = payload.initial or default_initial(size)
            local history = {}

            -- Gen 0
            local line = render_grid(_grid)
            history[#history + 1] = line
            orcs.output("[LifeGame] Gen 0: " .. line)

            -- Run generations
            for gen = 1, gens do
                _grid = step(_grid, size)
                line = render_grid(_grid)
                history[#history + 1] = line
                orcs.output("[LifeGame] Gen " .. gen .. ": " .. line)
            end

            local alive, kind_counts = stats(_grid)

            return {
                success = true,
                data = {
                    generations = gens,
                    grid_size   = size,
                    alive       = alive,
                    kind_counts = kind_counts,
                    history     = history,
                },
            }

        elseif op == "status" then
            local alive, kind_counts = stats(_grid)
            return {
                success = true,
                data = {
                    grid_size   = #_grid,
                    alive       = alive,
                    kind_counts = kind_counts,
                    grid        = render_grid(_grid),
                },
            }
        end

        return { success = false, error = "unknown operation: " .. op }
    end,

    on_signal = function(signal)
        if signal.kind == "Veto" then
            return "Abort"
        end
        return "Ignored"
    end,

    init = function()
        orcs.log("info", "life_game initialized")
        orcs.output("[LifeGame] Ready (max cells: " .. MAX_CELLS .. ")")
    end,

    shutdown = function()
        orcs.log("info", "life_game shutdown")
    end,
}

-- help.lua
-- HelpComponent: @component discovery and usage reference.
--
-- Queries agent_mgr for registered @routes and dynamic agents,
-- then queries individual components for their describe metadata.
--
-- Usage (via @component routing):
--   @help              → show usage
--   @help list         → list all @-routable components
--   @help <name>       → show detail for a specific component

--- Format a route list response for display.
local function format_route_list(routes, agents)
    local lines = { "[help] @-routable components:" }
    table.insert(lines, "")

    if routes and #routes > 0 then
        table.insert(lines, "  Built-in routes:")
        for _, r in ipairs(routes) do
            local desc = r.description or ""
            if desc ~= "" then
                desc = " - " .. desc
            end
            table.insert(lines, string.format("    @%-20s%s", r.prefix, desc))
        end
    end

    if agents and #agents > 0 then
        table.insert(lines, "")
        table.insert(lines, "  Dynamic agents:")
        for _, a in ipairs(agents) do
            local desc = a.expertise or ""
            if desc ~= "" then
                -- Truncate long expertise strings
                if #desc > 60 then
                    desc = desc:sub(1, 57) .. "..."
                end
                desc = " - " .. desc
            end
            table.insert(lines, string.format("    @%-20s%s", a.name, desc))
        end
    end

    if (not routes or #routes == 0) and (not agents or #agents == 0) then
        table.insert(lines, "  (none)")
    end

    table.insert(lines, "")
    table.insert(lines, "  Use @help <name> for detail.")
    return table.concat(lines, "\n")
end

--- Format a describe response for display.
local function format_describe(name, info)
    local lines = { string.format("[help] @%s", name) }

    if info.description and info.description ~= "" then
        table.insert(lines, "  " .. info.description)
    end

    if info.fqn and info.fqn ~= "" then
        table.insert(lines, string.format("  FQN: %s", info.fqn))
    end

    if info.operations and #info.operations > 0 then
        table.insert(lines, "  Operations:")
        for _, op in ipairs(info.operations) do
            table.insert(lines, "    - " .. op)
        end
    end

    if info.usage and info.usage ~= "" then
        table.insert(lines, "  Usage:")
        table.insert(lines, "    " .. info.usage)
    end

    return table.concat(lines, "\n")
end

--- Handle the "list" operation: query agent_mgr for all routes.
local function handle_list()
    local resp = orcs.request("builtin::agent_mgr", "list_routes", {})
    if not resp or not resp.success then
        local err = (resp and resp.error) or "failed to query agent_mgr"
        orcs.output("[help] Error: " .. err)
        return { success = false, error = err }
    end

    local data = resp.data or {}
    local text = format_route_list(data.routes, data.agents)
    orcs.output(text)
    return { success = true, data = data }
end

--- Handle the "describe" operation: query a specific component.
local function handle_describe(name)
    -- First ask agent_mgr for the target FQN
    local resp = orcs.request("builtin::agent_mgr", "resolve_route", { name = name })
    if not resp or not resp.success then
        orcs.output(string.format("[help] Unknown component: @%s", name))
        return { success = false, error = "unknown component: " .. name }
    end

    local target_fqn = resp.data and resp.data.target
    if not target_fqn then
        orcs.output(string.format("[help] Cannot resolve @%s", name))
        return { success = false, error = "cannot resolve: " .. name }
    end

    -- Query the target component for describe metadata
    local desc_resp = orcs.request(target_fqn, "describe", {})
    local info
    if desc_resp and desc_resp.success and desc_resp.data then
        info = desc_resp.data
    else
        -- Fallback: minimal info from resolve_route
        info = {
            description = resp.data.description or "",
            fqn = target_fqn,
            operations = {},
        }
    end
    info.fqn = info.fqn or target_fqn

    local text = format_describe(name, info)
    orcs.output(text)
    return { success = true, data = info }
end

--- Handle free-text input from @help routing.
local function handle_input(payload)
    local msg = payload
    if type(msg) == "table" then
        msg = msg.message or ""
    end
    if type(msg) ~= "string" or msg:match("^%s*$") then
        orcs.output("[help] Usage: @help list | @help <component>")
        return { success = true }
    end

    local trimmed = msg:match("^%s*(.-)%s*$")

    if trimmed == "list" then
        return handle_list()
    else
        return handle_describe(trimmed)
    end
end

--- Return self-describe metadata (called via RPC from other components).
local function handle_self_describe(_payload)
    return {
        success = true,
        data = {
            id = "help",
            fqn = "builtin::help",
            description = "Component discovery and usage reference. Lists @-routable components and their details.",
            operations = { "list", "describe" },
            usage = "@help list | @help <component>",
        },
    }
end

-- === Handler dispatch table ===
local handlers = {
    list     = handle_list,
    describe = handle_self_describe,
    input    = handle_input,
}

-- === Component Definition ===
return {
    id = "help",
    namespace = "builtin",
    description = "Component discovery and usage reference. Use @help list to see all @-routable components.",
    subscriptions = {},
    elevated = false,
    output_to_io = true,

    init = function()
        orcs.log("info", "HelpComponent initialized")
    end,

    shutdown = function()
        orcs.log("info", "HelpComponent shutdown")
    end,

    on_request = function(request)
        local operation = request.operation or "input"
        local handler = handlers[operation]
        if handler then
            return handler(request.payload)
        end

        -- Fallback: treat operation as describe target
        return handle_describe(operation)
    end,

    on_signal = function(signal)
        if signal.kind == "Veto" then
            return "Abort"
        end
        return "Ignored"
    end,
}

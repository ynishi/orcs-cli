-- profile_manager.lua
-- ProfileManagementComponent: Discovery / Loading / Switching of Profiles.
--
-- Profiles bundle config overrides and per-component settings
-- into a switchable unit. Component settings are distributed
-- via orcs.request() to target components by last-name or FQL.
--
-- Operations:
--   list     -> list available profiles
--   current  -> get active profile name
--   use      -> switch to a profile (distributes component settings)
--   get      -> get a profile definition
--   reload   -> re-scan profile directories

-- Module-level state
local active_profile = nil   -- Currently active profile name
local active_def = nil       -- Currently active ProfileDef (parsed TOML)
local profiles_cache = {}    -- name -> { path, def }

-- === Helpers ===

--- Build the ordered list of profile search directories.
--- Project profiles (highest priority) come first, then global.
local function get_search_dirs()
    local dirs = {
        orcs.pwd .. "/.orcs/profiles",
    }
    local home = os.getenv("HOME")
    if home then
        table.insert(dirs, home .. "/.orcs/profiles")
    end
    return dirs
end

--- Validate a profile name (no path traversal characters).
--- Returns true if valid, false + error string if not.
local function validate_profile_name(name)
    if not name or name == "" then
        return false, "profile name is empty"
    end
    if name:find("[/\\]") or name:find("%.%.") then
        return false, "invalid profile name (must not contain '/', '\\', or '..'): " .. name
    end
    return true
end

--- Discover profile TOML files from well-known directories.
--- Returns array of { name, path, description }
local function discover_profiles()
    local entries = {}
    local seen = {}

    for _, dir in ipairs(get_search_dirs()) do
        local result = orcs.glob("*.toml", dir)
        if result and result.ok and result.files then
            for _, file_path in ipairs(result.files) do
                -- Extract name from filename (strip .toml)
                local name = file_path:match("([^/]+)%.toml$")
                if name and not seen[name] then
                    -- Try to parse for metadata
                    local read_result = orcs.read(file_path)
                    if read_result and read_result.ok and read_result.content then
                        local ok, def = pcall(orcs.toml_parse, read_result.content)
                        if ok and def then
                            local profile_name = (def.profile and def.profile.name) or name
                            local description = (def.profile and def.profile.description) or ""

                            table.insert(entries, {
                                name = profile_name,
                                description = description,
                                path = file_path,
                            })

                            -- Cache the parsed definition
                            profiles_cache[profile_name] = {
                                path = file_path,
                                def = def,
                            }

                            seen[name] = true
                        end
                    end
                end
            end
        end
    end

    return entries
end

--- Load a profile definition by name.
--- Returns def table or nil, error_string
local function load_profile(name)
    -- Validate name to prevent path traversal
    local valid, verr = validate_profile_name(name)
    if not valid then
        return nil, verr
    end

    -- Check cache first
    if profiles_cache[name] and profiles_cache[name].def then
        return profiles_cache[name].def
    end

    for _, dir in ipairs(get_search_dirs()) do
        local path = dir .. "/" .. name .. ".toml"
        local result = orcs.read(path)
        if result and result.ok and result.content then
            local ok, def = pcall(orcs.toml_parse, result.content)
            if ok and def then
                -- Default name from filename
                if not def.profile then
                    def.profile = {}
                end
                if not def.profile.name or def.profile.name == "" then
                    def.profile.name = name
                end

                profiles_cache[name] = { path = path, def = def }
                return def
            else
                return nil, "failed to parse profile: " .. tostring(def)
            end
        end
    end

    return nil, "profile not found: " .. name
end

--- Distribute component settings from a profile.
--- Sends orcs.request to each target component with their settings.
local function distribute_component_settings(def)
    if not def.components then
        return {}
    end

    local results = {}
    for target, settings in pairs(def.components) do
        -- Determine the request target FQL
        -- If target contains "::", treat as FQL; otherwise use last-name lookup
        local fql
        if target:find("::") then
            fql = target
        else
            -- Convention: namespace::component_name
            -- Try common patterns; fall back to Extension category request
            fql = target
        end

        -- Send profile_apply request to the target component
        local ok, resp = pcall(function()
            return orcs.request(fql, "profile_apply", {
                profile = def.profile and def.profile.name or "unknown",
                settings = settings,
            })
        end)

        if ok and resp and resp.success then
            table.insert(results, {
                target = target,
                status = "applied",
            })
            orcs.log("info", string.format(
                "ProfileManager: applied settings to @%s", target
            ))
        else
            local err = "unknown error"
            if not ok then
                err = tostring(resp)
            elseif resp and resp.error then
                err = resp.error
            end
            table.insert(results, {
                target = target,
                status = "failed",
                error = err,
            })
            orcs.log("warn", string.format(
                "ProfileManager: failed to apply to @%s: %s", target, err
            ))
        end
    end

    return results
end

-- === Request Handlers ===

local function handle_list(_payload)
    local entries = discover_profiles()
    return {
        success = true,
        data = entries,
        count = #entries,
        active = active_profile,
    }
end

local function handle_current(_payload)
    if not active_profile then
        return { success = true, data = { active = false } }
    end
    return {
        success = true,
        data = {
            active = true,
            name = active_profile,
            description = active_def and active_def.profile and active_def.profile.description or "",
            components = active_def and active_def.components and
                (function()
                    local names = {}
                    for k in pairs(active_def.components) do
                        table.insert(names, k)
                    end
                    return names
                end)() or {},
        },
    }
end

local function handle_use(payload)
    if not payload or not payload.name then
        return { success = false, error = "payload.name is required" }
    end

    local name = payload.name
    local def, err = load_profile(name)
    if not def then
        return { success = false, error = err }
    end

    -- Distribute component settings
    local results = distribute_component_settings(def)

    -- Track active profile
    active_profile = def.profile and def.profile.name or name
    active_def = def

    orcs.log("info", string.format(
        "ProfileManager: switched to profile '%s'", active_profile
    ))

    return {
        success = true,
        data = {
            name = active_profile,
            description = def.profile and def.profile.description or "",
            config_applied = def.config ~= nil,
            component_results = results,
        },
    }
end

local function handle_get(payload)
    if not payload or not payload.name then
        return { success = false, error = "payload.name is required" }
    end

    local def, err = load_profile(payload.name)
    if not def then
        return { success = false, error = err }
    end

    return { success = true, data = def }
end

local function handle_reload(_payload)
    profiles_cache = {}
    local entries = discover_profiles()
    return {
        success = true,
        data = {
            discovered = #entries,
            profiles = entries,
        },
    }
end

--- Format a profile list response for display.
local function format_list(resp)
    if not resp.success then
        return "[profile] Error: " .. (resp.error or "unknown")
    end
    local lines = { "[profile] Available profiles:" }
    if resp.data and #resp.data > 0 then
        for _, entry in ipairs(resp.data) do
            local marker = (resp.active == entry.name) and " *" or "  "
            local desc = entry.description ~= "" and (" - " .. entry.description) or ""
            table.insert(lines, string.format("%s %s%s", marker, entry.name, desc))
        end
    else
        table.insert(lines, "  (none)")
    end
    return table.concat(lines, "\n")
end

--- Format a current-profile response for display.
local function format_current(resp)
    if not resp.success then
        return "[profile] Error: " .. (resp.error or "unknown")
    end
    if resp.data and resp.data.active then
        return string.format("[profile] Active: %s", resp.data.name)
    end
    return "[profile] No active profile"
end

--- Format a use-profile response for display.
local function format_use(resp)
    if not resp.success then
        return "[profile] Error: " .. (resp.error or "unknown")
    end
    local d = resp.data or {}
    local parts = { string.format("[profile] Switched to '%s'", d.name or "?") }
    if d.component_results then
        for _, r in ipairs(d.component_results) do
            table.insert(parts, string.format("  @%s: %s", r.target, r.status))
        end
    end
    return table.concat(parts, "\n")
end

--- Handle free-text input from @profile_manager routing.
--- Parses "op [arg]" and dispatches to the matching handler.
local function handle_input(payload)
    local msg = payload
    if type(msg) == "table" then
        msg = msg.message or ""
    end
    if type(msg) ~= "string" or msg == "" then
        orcs.output("[profile] Usage: list | current | use <name> | get <name> | reload")
        return { success = true }
    end

    local op, arg = msg:match("^(%S+)%s*(.*)")
    if not op then
        orcs.output("[profile] Unknown command: " .. msg)
        return { success = false, error = "unknown command: " .. msg }
    end

    -- Dispatch table (input excluded to avoid loop)
    local dispatch = {
        list    = handle_list,
        current = handle_current,
        ["use"] = handle_use,
        get     = handle_get,
        reload  = handle_reload,
    }

    local formatters = {
        list    = format_list,
        current = format_current,
        ["use"] = format_use,
    }

    local target = dispatch[op]
    if target then
        local cmd_payload = {}
        if arg and arg ~= "" then
            cmd_payload.name = arg:match("^%s*(.-)%s*$") -- trim
        end
        local resp = target(cmd_payload)

        -- Display formatted output
        local formatter = formatters[op]
        if formatter then
            orcs.output(formatter(resp))
        elseif resp.success then
            orcs.output("[profile] OK: " .. orcs.json_encode(resp.data or {}))
        else
            orcs.output("[profile] Error: " .. (resp.error or "unknown"))
        end

        return resp
    end

    orcs.output("[profile] Unknown operation: " .. op)
    return { success = false, error = "unknown operation: " .. op }
end

-- === Handler dispatch table ===
local handlers = {
    list    = handle_list,
    current = handle_current,
    use     = handle_use,
    get     = handle_get,
    reload  = handle_reload,
    input   = handle_input,
}

-- === Component Definition ===
return {
    id = "profile_manager",
    namespace = "profile",
    subscriptions = { "Extension" },
    elevated = true,
    output_to_io = true,

    init = function()
        -- Auto-discover available profiles
        local entries = discover_profiles()
        orcs.log("info", string.format(
            "ProfileManager initialized (profiles: %d)", #entries
        ))

        -- Log discovered profiles
        for _, entry in ipairs(entries) do
            orcs.log("debug", string.format(
                "  profile: %s - %s (%s)",
                entry.name, entry.description, entry.path
            ))
        end
    end,

    shutdown = function()
        active_profile = nil
        active_def = nil
        profiles_cache = {}
        orcs.log("info", "ProfileManager shutdown")
    end,

    on_request = function(request)
        local handler = handlers[request.operation]
        if handler then
            return handler(request.payload)
        end
        return { success = false, error = "unknown operation: " .. request.operation }
    end,

    on_signal = function(signal)
        if signal.kind == "Veto" then
            return "Abort"
        end
        -- ProfileManager has no approval flow; Approve/Reject pass through.
        return "Handled"
    end,

    snapshot = function()
        return {
            active_profile = active_profile,
        }
    end,

    restore = function(state)
        if not state then return end

        -- Re-discover profiles so cache is populated
        discover_profiles()

        -- Restore active profile by re-applying it
        if state.active_profile then
            local def, err = load_profile(state.active_profile)
            if def then
                distribute_component_settings(def)
                active_profile = def.profile and def.profile.name or state.active_profile
                active_def = def
                orcs.log("info", string.format(
                    "ProfileManager restored profile '%s'", active_profile
                ))
            else
                orcs.log("warn", string.format(
                    "ProfileManager: could not restore profile '%s': %s",
                    state.active_profile, err or "unknown"
                ))
            end
        end
    end,
}

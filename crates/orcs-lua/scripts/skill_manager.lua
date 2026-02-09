-- skill_manager.lua
-- SkillManagementComponent: Discovery / Registration / Activation / Execution
-- of Agent Skills (Composable Prompt-Level Plugins).
--
-- Uses lib modules loaded via orcs.require_lib().

-- Load library modules
local FormatAdapter = orcs.require_lib("format_adapter")
local SkillRegistry = orcs.require_lib("skill_registry")
local SkillLoader   = orcs.require_lib("skill_loader")
local SkillCatalog  = orcs.require_lib("skill_catalog")

-- Module-level state (initialized in init())
local registry = nil
local catalog = nil

-- === Request Handlers ===

-- List all skills (L1 catalog entries)
local function handle_list(payload)
    local filter = payload and payload.filter
    local entries
    if filter then
        entries = registry:search(filter)
    else
        entries = registry:list()
    end
    return { success = true, data = entries, count = #entries }
end

-- Get a single skill by name
local function handle_get(payload)
    if not payload or not payload.name then
        return { success = false, error = "payload.name is required" }
    end
    local skill = registry:get(payload.name)
    if not skill then
        return { success = false, error = "skill not found: " .. payload.name }
    end
    return { success = true, data = skill }
end

-- Search skills by query
local function handle_search(payload)
    if not payload or not payload.query then
        return { success = false, error = "payload.query is required" }
    end
    local entries = registry:search(payload.query)
    return { success = true, data = entries, count = #entries }
end

-- Register a skill manually
local function handle_register(payload)
    if not payload or not payload.skill then
        return { success = false, error = "payload.skill is required" }
    end
    local ok, err = registry:register(payload.skill)
    if not ok then
        return { success = false, error = err }
    end
    return { success = true, data = { count = registry:count() } }
end

-- Unregister a skill
local function handle_unregister(payload)
    if not payload or not payload.name then
        return { success = false, error = "payload.name is required" }
    end
    local ok, err = registry:unregister(payload.name)
    if not ok then
        return { success = false, error = err }
    end
    return { success = true, data = { count = registry:count() } }
end

-- Activate a skill (L2: load body)
local function handle_activate(payload)
    if not payload or not payload.name then
        return { success = false, error = "payload.name is required" }
    end
    local skill, err = catalog:activate(payload.name)
    if not skill then
        return { success = false, error = err }
    end
    return {
        success = true,
        data = {
            name = skill.name,
            body = skill.body,
            token_estimate = skill.token_estimate,
            state = skill.state,
        },
    }
end

-- Deactivate a skill (L2: release body)
local function handle_deactivate(payload)
    if not payload or not payload.name then
        return { success = false, error = "payload.name is required" }
    end
    local ok, err = catalog:deactivate(payload.name)
    if not ok then
        return { success = false, error = err or "deactivation failed" }
    end
    return { success = true }
end

-- Load resources (L3)
local function handle_resources(payload)
    if not payload or not payload.name or not payload.type then
        return { success = false, error = "payload.name and payload.type are required" }
    end
    local resources, err = catalog:load_resources(payload.name, payload.type)
    if not resources then
        return { success = false, error = err }
    end
    return { success = true, data = resources }
end

-- Discover skills from a directory
local function handle_discover(payload)
    if not payload or not payload.path then
        return { success = false, error = "payload.path is required" }
    end
    local skills, errors = SkillLoader.load_dir(payload.path, payload, FormatAdapter)
    local registered = 0
    for _, skill in ipairs(skills) do
        local ok = registry:register(skill)
        if ok then registered = registered + 1 end
    end
    return {
        success = true,
        data = {
            discovered = #skills,
            registered = registered,
            errors = #errors > 0 and errors or nil,
        },
    }
end

-- Render catalog string (within budget)
local function handle_catalog(payload)
    local text, stats = catalog:render_catalog(payload)
    return { success = true, data = { catalog = text, stats = stats } }
end

-- Load from directory (discover + register)
local function handle_load_dir(payload)
    return handle_discover(payload)
end

-- Load from single file
local function handle_load_file(payload)
    if not payload or not payload.path then
        return { success = false, error = "payload.path is required" }
    end
    local skill, err = SkillLoader.load_file(payload.path, FormatAdapter)
    if not skill then
        return { success = false, error = err }
    end
    local ok, reg_err = registry:register(skill)
    if not ok then
        return { success = false, error = reg_err }
    end
    return { success = true, data = { skill = skill.name } }
end

-- Reload (hot reload)
local function handle_reload(payload)
    if not payload or not payload.path then
        return { success = false, error = "payload.path is required" }
    end
    local skills, errors = SkillLoader.load_dir(payload.path, payload, FormatAdapter)
    local stats = registry:reload(skills)
    return {
        success = true,
        data = {
            stats = stats,
            errors = #errors > 0 and errors or nil,
        },
    }
end

-- Detect format of a path
local function handle_detect_format(payload)
    if not payload or not payload.path then
        return { success = false, error = "payload.path is required" }
    end
    local format = SkillLoader.detect_format(payload.path)
    return { success = true, data = { format = format } }
end

-- Active skills list
local function handle_active(payload)
    local skills = catalog:active_skills()
    local entries = {}
    for _, s in ipairs(skills) do
        table.insert(entries, {
            name = s.name,
            token_estimate = s.token_estimate,
            activated_at = s.activated_at,
        })
    end
    return { success = true, data = entries, count = #entries }
end

-- Status
local function handle_status(_payload)
    return {
        success = true,
        data = {
            total = registry:count(),
            frozen = registry.frozen,
            active = catalog:active_count(),
            formats = FormatAdapter.supported_formats(),
        },
    }
end

-- === Handler dispatch table ===
local handlers = {
    -- Registry
    list       = handle_list,
    get        = handle_get,
    search     = handle_search,
    register   = handle_register,
    unregister = handle_unregister,
    -- Activation / Disclosure
    activate   = handle_activate,
    deactivate = handle_deactivate,
    resources  = handle_resources,
    active     = handle_active,
    -- Catalog
    discover   = handle_discover,
    catalog    = handle_catalog,
    -- Loader
    load_dir   = handle_load_dir,
    load_file  = handle_load_file,
    reload     = handle_reload,
    -- Format
    detect_format = handle_detect_format,
    -- Meta
    status     = handle_status,
}

-- === Component Definition ===
return {
    id = "skill_manager",
    namespace = "skill",
    subscriptions = { "Extension" },
    elevated = true,

    init = function()
        registry = SkillRegistry.new()
        catalog = SkillCatalog.new(registry)

        -- Auto-discover skills from well-known paths
        local discover_paths = {
            orcs.pwd .. "/.orcs/skills",
        }
        local total_discovered = 0
        for _, path in ipairs(discover_paths) do
            local result = handle_discover({ path = path })
            if result.success and result.data then
                local n = result.data.registered or 0
                total_discovered = total_discovered + n
                if n > 0 then
                    orcs.log("info", string.format(
                        "SkillManager: discovered %d skills from %s", n, path
                    ))
                end
            end
        end

        orcs.log("info", string.format(
            "SkillManager initialized (skills: %d)", total_discovered
        ))
    end,

    shutdown = function()
        registry = nil
        catalog = nil
        orcs.log("info", "SkillManager shutdown")
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
        return "Handled"
    end,
}

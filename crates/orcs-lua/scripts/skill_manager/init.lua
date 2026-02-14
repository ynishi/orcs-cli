-- skill_manager.lua
-- SkillManagementComponent: Discovery / Registration / Activation / Execution
-- of Agent Skills (Composable Prompt-Level Plugins).

-- Load library modules
local FormatAdapter = require("format_adapter")
local SkillRegistry = require("skill_registry")
local SkillLoader   = require("skill_loader")
local SkillCatalog  = require("skill_catalog")

-- Module-level state (initialized in init())
local registry = nil
local catalog = nil

-- Component settings (populated from config in init())
local component_settings = {}

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

-- Select top-N relevant skills for a query
local function handle_select(payload)
    local query = payload and payload.query or ""
    local limit = payload and payload.limit or 5
    local entries = registry:select(query, limit)
    return { success = true, data = entries, count = #entries }
end

-- Recommend skills for a user intent using lightweight LLM judgment.
-- Returns 0-5 skills with name + description, filtered by relevance.
--
-- payload:
--   intent  (string)  — user's message / intent
--   context (string)  — recent conversation context (optional)
--   limit   (number)  — max skills to return (default 5)
--   model   (string)  — LLM model for judgment (default haiku)
local function handle_recommend(payload)
    -- Check if recommend is disabled via config
    if component_settings.recommend_skill == false then
        -- Fallback to keyword select when LLM-based recommend is disabled
        local intent = payload and payload.intent or ""
        local limit = payload and payload.limit or 5
        local entries = registry:select(intent, limit)
        return { success = true, data = entries, count = #entries, fallback = true }
    end

    if not payload or not payload.intent then
        return { success = false, error = "payload.intent is required" }
    end

    local intent = payload.intent
    local context = payload.context or ""
    local limit = payload.limit or 5

    -- Gather all active skills (L1 catalog entries)
    local all_skills = catalog:active_skills()
    if #all_skills == 0 then
        return { success = true, data = {}, count = 0 }
    end

    -- Build skill catalog for LLM prompt
    local skill_lines = {}
    for i, s in ipairs(all_skills) do
        local desc = s.description or ""
        -- Truncate description to ~60 words for prompt efficiency
        if #desc > 300 then
            desc = desc:sub(1, 300) .. "..."
        end
        skill_lines[i] = string.format("%d. %s — %s", i, s.name, desc)
    end
    local skill_list = table.concat(skill_lines, "\n")

    -- Build LLM prompt for skill selection (table.concat to avoid % escaping issues)
    local prompt_parts = {
        "You are a skill recommender. Given a user's intent and available skills, select the most relevant skills.",
        "",
        "## User Intent",
        intent,
    }
    if context ~= "" then
        prompt_parts[#prompt_parts + 1] = ""
        prompt_parts[#prompt_parts + 1] = "## Recent Context"
        prompt_parts[#prompt_parts + 1] = context
    end
    prompt_parts[#prompt_parts + 1] = ""
    prompt_parts[#prompt_parts + 1] = "## Available Skills (" .. #all_skills .. " total)"
    prompt_parts[#prompt_parts + 1] = skill_list
    prompt_parts[#prompt_parts + 1] = ""
    prompt_parts[#prompt_parts + 1] = "## Instructions"
    prompt_parts[#prompt_parts + 1] = "- Select 0-" .. limit .. " skills that are directly relevant to the user's intent."
    prompt_parts[#prompt_parts + 1] = "- If no skill is relevant, return an empty list."
    prompt_parts[#prompt_parts + 1] = '- Return ONLY a JSON array of skill names (strings). Example: ["skill-a", "skill-b"]'
    prompt_parts[#prompt_parts + 1] = "- If none are relevant, return: []"
    prompt_parts[#prompt_parts + 1] = "- No explanation needed, just the JSON array."
    local prompt = table.concat(prompt_parts, "\n")

    -- Call lightweight LLM (haiku by default)
    local model = payload.model or "claude-haiku-4-5-20251001"
    local llm_resp = orcs.llm(prompt, { model = model })

    if not llm_resp or not llm_resp.ok then
        -- Fallback to keyword-based select if LLM fails
        orcs.log("warn", "SkillManager recommend: LLM failed, falling back to keyword select")
        local entries = registry:select(intent, limit)
        return { success = true, data = entries, count = #entries, fallback = true }
    end

    -- Parse LLM response: expect JSON array of skill names
    local content = llm_resp.content or ""
    -- Strip markdown code blocks (```json ... ```) that LLMs sometimes wrap around output
    local stripped = content:gsub("```json%s*", ""):gsub("```%s*", "")
    -- Greedy match: first '[' to last ']' — safe because LLM is asked to return only a JSON array
    local json_str = stripped:match("%[.*%]")
    if not json_str then
        orcs.log("warn", "SkillManager recommend: could not parse LLM response, fallback")
        local entries = registry:select(intent, limit)
        return { success = true, data = entries, count = #entries, fallback = true }
    end

    -- Parse the JSON array of names
    local ok, names = pcall(orcs.json_parse, json_str)
    if not ok or type(names) ~= "table" then
        orcs.log("warn", "SkillManager recommend: JSON parse failed, fallback")
        local entries = registry:select(intent, limit)
        return { success = true, data = entries, count = #entries, fallback = true }
    end

    -- Resolve names to catalog entries (with description)
    local recommended = {}
    for _, name in ipairs(names) do
        if #recommended >= limit then break end
        local skill = registry:get(name)
        if skill then
            table.insert(recommended, {
                name = skill.name,
                description = skill.description or "",
                token_estimate = skill.token_estimate,
            })
        end
    end

    orcs.log("info", string.format(
        "SkillManager recommend: intent=%s → %d skills (of %d total)",
        intent:sub(1, 50), #recommended, #all_skills
    ))

    return { success = true, data = recommended, count = #recommended }
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

-- Apply profile settings (activate/deactivate skills)
local function handle_profile_apply(payload)
    if not payload or not payload.settings then
        return { success = false, error = "payload.settings is required" }
    end
    local settings = payload.settings
    local results = { activated = {}, deactivated = {} }

    if settings.activate then
        for _, name in ipairs(settings.activate) do
            local ok, err = catalog:activate(name)
            if ok then
                table.insert(results.activated, name)
            else
                orcs.log("warn", string.format(
                    "SkillManager: profile_apply activate '%s' failed: %s",
                    name, err or "unknown"
                ))
            end
        end
    end

    if settings.deactivate then
        for _, name in ipairs(settings.deactivate) do
            local ok, err = catalog:deactivate(name)
            if ok then
                table.insert(results.deactivated, name)
            else
                orcs.log("warn", string.format(
                    "SkillManager: profile_apply deactivate '%s' failed: %s",
                    name, err or "unknown"
                ))
            end
        end
    end

    orcs.log("info", string.format(
        "SkillManager: profile '%s' applied (activated: %d, deactivated: %d)",
        payload.profile or "?",
        #results.activated, #results.deactivated
    ))

    return { success = true, data = results }
end

-- === Handler dispatch table ===
local handlers = {
    -- Registry
    list       = handle_list,
    get        = handle_get,
    search     = handle_search,
    register   = handle_register,
    unregister = handle_unregister,
    -- Selection
    select     = handle_select,
    recommend  = handle_recommend,
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
    -- Profile integration
    profile_apply = handle_profile_apply,
}

-- === Component Definition ===
return {
    id = "skill_manager",
    namespace = "skill",
    -- All skill_manager operations are RPC-based (direct events).
    -- No broadcast Extension events are needed, so no Extension subscription.
    -- To subscribe to specific Extension operations, use table form:
    --   { category = "Extension", operations = {"route_response"} }
    subscriptions = {},
    elevated = true,

    init = function(cfg)
        -- Store component settings from [components.settings.skill_manager]
        if cfg and type(cfg) == "table" then
            component_settings = cfg
            orcs.log("debug", "SkillManager: received config: " .. orcs.json_encode(cfg))
        end

        -- If restore() was already called, registry is set; skip re-init
        if registry then
            orcs.log("info", string.format(
                "SkillManager init: restored session (skills: %d, active: %d)",
                registry:count(), catalog:active_count()
            ))
            return
        end

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

        -- Auto-activate all discovered skills (L2)
        local all_skills = registry:list()
        local activated = 0
        for _, entry in ipairs(all_skills) do
            local ok, err = catalog:activate(entry.name)
            if ok then
                activated = activated + 1
            else
                orcs.log("warn", string.format(
                    "SkillManager: failed to activate '%s': %s",
                    entry.name, err or "unknown"
                ))
            end
        end

        orcs.log("info", string.format(
            "SkillManager initialized (skills: %d, active: %d)",
            total_discovered, activated
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

    snapshot = function()
        if not registry then return {} end
        local data = registry:serialize()
        -- Include activated skill names for catalog rebuild
        local active_names = {}
        if catalog then
            for name in pairs(catalog.activated) do
                table.insert(active_names, name)
            end
        end
        data.active_names = active_names
        return data
    end,

    restore = function(state)
        if not state then return end
        registry = SkillRegistry.new()
        registry:deserialize(state)
        catalog = SkillCatalog.new(registry)
        -- Rebuild activated set from snapshot
        if state.active_names then
            for _, name in ipairs(state.active_names) do
                local skill = registry:get(name)
                if skill and skill.state == "activated" then
                    catalog.activated[name] = true
                end
            end
        end
        orcs.log("info", string.format(
            "SkillManager restored (skills: %d, active: %d)",
            registry:count(), catalog:active_count()
        ))
    end,
}

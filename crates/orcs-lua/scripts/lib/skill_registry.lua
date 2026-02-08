-- skill_registry.lua
-- Manages skill registration, search, and freeze/reload lifecycle.

local SkillRegistry = {}
SkillRegistry.__index = SkillRegistry

function SkillRegistry.new()
    return setmetatable({
        skills = {},          -- name -> Skill
        frozen = false,       -- freeze() blocks register/unregister
        load_order = {},      -- registration order
    }, SkillRegistry)
end

-- Validate a skill table has required fields
local function validate_skill(skill)
    if not skill.name or type(skill.name) ~= "string" then
        return false, "skill.name is required and must be a string"
    end
    if #skill.name > 64 then
        return false, "skill.name must be <= 64 characters"
    end
    if not skill.source or not skill.source.format then
        return false, "skill.source.format is required"
    end
    return true
end

-- Convert full skill to lightweight catalog entry (L1)
local function to_catalog_entry(skill)
    local fm = skill.frontmatter or {}
    local meta = skill.metadata or {}
    return {
        name = skill.name,
        description = skill.description,
        source_format = skill.source and skill.source.format,
        categories = meta.categories or {},
        tags = meta.tags or {},
        user_invocable = fm["user-invocable"] ~= false,
        model_invocable = fm["disable-model-invocation"] ~= true,
        token_estimate = skill.token_estimate,
        state = skill.state,
    }
end

-- Register a skill
function SkillRegistry:register(skill)
    if self.frozen then
        return false, "registry is frozen"
    end
    local ok, err = validate_skill(skill)
    if not ok then return false, err end
    if self.skills[skill.name] then
        return false, "skill already registered: " .. skill.name
    end
    self.skills[skill.name] = skill
    table.insert(self.load_order, skill.name)
    return true
end

-- Get a skill by name
function SkillRegistry:get(name)
    return self.skills[name]
end

-- List skills as catalog entries (L1 lightweight view)
function SkillRegistry:list(filter)
    local entries = {}
    for _, name in ipairs(self.load_order) do
        local skill = self.skills[name]
        if skill and (not filter or filter(skill)) then
            table.insert(entries, to_catalog_entry(skill))
        end
    end
    return entries
end

-- Search skills by query string (matches name, description, tags, categories)
function SkillRegistry:search(query)
    if not query or query == "" then
        return self:list()
    end
    local q = query:lower()
    return self:list(function(skill)
        if skill.name:lower():find(q, 1, true) then return true end
        if skill.description and skill.description:lower():find(q, 1, true) then return true end
        local meta = skill.metadata or {}
        for _, tag in ipairs(meta.tags or {}) do
            if tag:lower():find(q, 1, true) then return true end
        end
        for _, cat in ipairs(meta.categories or {}) do
            if cat:lower():find(q, 1, true) then return true end
        end
        return false
    end)
end

-- Freeze the registry (blocks register/unregister, allows activate/deactivate)
function SkillRegistry:freeze()
    self.frozen = true
end

-- Unregister a skill by name
function SkillRegistry:unregister(name)
    if self.frozen then
        return false, "registry is frozen"
    end
    if not self.skills[name] then
        return false, "skill not found: " .. name
    end
    self.skills[name] = nil
    for i, n in ipairs(self.load_order) do
        if n == name then
            table.remove(self.load_order, i)
            break
        end
    end
    return true
end

-- Skill count
function SkillRegistry:count()
    return #self.load_order
end

-- Hot reload: temporarily thaw, apply diffs, re-freeze
function SkillRegistry:reload(new_skills)
    local was_frozen = self.frozen
    self.frozen = false

    local current_names = {}
    for _, name in ipairs(self.load_order) do
        current_names[name] = true
    end

    local registered = 0
    local updated = 0
    for _, skill in ipairs(new_skills) do
        if self.skills[skill.name] then
            -- Update existing: L1 metadata only
            self.skills[skill.name].description = skill.description
            self.skills[skill.name].frontmatter = skill.frontmatter
            self.skills[skill.name].metadata = skill.metadata
            updated = updated + 1
        else
            self:register(skill)
            registered = registered + 1
        end
        current_names[skill.name] = nil
    end

    -- Remove skills that no longer exist
    local removed = 0
    for name in pairs(current_names) do
        self:unregister(name)
        removed = removed + 1
    end

    if was_frozen then
        self.frozen = true
    end

    return { registered = registered, updated = updated, removed = removed }
end

return SkillRegistry

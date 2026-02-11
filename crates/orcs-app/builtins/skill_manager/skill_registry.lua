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

-- Select top-N skills relevant to a query, scored by match quality.
-- Returns catalog entries sorted by score (descending), limited to `limit`.
function SkillRegistry:select(query, limit)
    limit = limit or 5
    if not query or query == "" then
        -- No query: return first N skills
        local entries = self:list()
        local result = {}
        for i = 1, math.min(limit, #entries) do
            result[i] = entries[i]
        end
        return result
    end

    -- Tokenize query into lowercase words
    local words = {}
    for w in query:lower():gmatch("%w+") do
        words[#words + 1] = w
    end

    -- Score each skill
    local scored = {}
    for _, name in ipairs(self.load_order) do
        local skill = self.skills[name]
        if skill then
            local score = 0
            local sname = skill.name:lower()
            local sdesc = (skill.description or ""):lower()
            local meta = skill.metadata or {}
            local tags_str = table.concat(meta.tags or {}, " "):lower()
            local cats_str = table.concat(meta.categories or {}, " "):lower()

            for _, w in ipairs(words) do
                -- Name exact match: highest weight
                if sname == w then
                    score = score + 10
                elseif sname:find(w, 1, true) then
                    score = score + 5
                end
                -- Description match
                if sdesc:find(w, 1, true) then
                    score = score + 3
                end
                -- Tags match
                if tags_str:find(w, 1, true) then
                    score = score + 2
                end
                -- Categories match
                if cats_str:find(w, 1, true) then
                    score = score + 1
                end
            end

            if score > 0 then
                scored[#scored + 1] = { entry = to_catalog_entry(skill), score = score }
            end
        end
    end

    -- Sort by score descending
    table.sort(scored, function(a, b) return a.score > b.score end)

    -- Return top-N
    local result = {}
    for i = 1, math.min(limit, #scored) do
        local item = scored[i].entry
        item.score = scored[i].score
        result[i] = item
    end
    return result
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

-- Serialize registry state for snapshot persistence.
-- Returns a plain table (no metatables) suitable for JSON serialization.
function SkillRegistry:serialize()
    local skill_list = {}
    for _, name in ipairs(self.load_order) do
        local skill = self.skills[name]
        if skill then
            table.insert(skill_list, {
                name = skill.name,
                description = skill.description,
                source = skill.source,
                frontmatter = skill.frontmatter,
                metadata = skill.metadata,
                state = skill.state,
                body = skill.body,
                token_estimate = skill.token_estimate,
            })
        end
    end
    return {
        skills = skill_list,
        frozen = self.frozen,
    }
end

-- Restore registry state from a serialized snapshot.
-- Idempotent: replaces current state entirely (no append).
function SkillRegistry:deserialize(data)
    self.skills = {}
    self.load_order = {}
    self.frozen = false

    if data and data.skills then
        for _, skill in ipairs(data.skills) do
            self:register(skill)
        end
    end
    self.frozen = data and data.frozen or false
end

return SkillRegistry

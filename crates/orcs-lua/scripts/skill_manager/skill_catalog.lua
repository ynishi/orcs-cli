-- skill_catalog.lua
-- Progressive Disclosure management for skills (L1/L2/L3).

local SkillCatalog = {}
SkillCatalog.__index = SkillCatalog

function SkillCatalog.new(registry)
    return setmetatable({
        registry = registry,
        budget_chars = 16000,    -- default: ~2% of context
        activated = {},          -- name -> true (L2 loaded)
    }, SkillCatalog)
end

-- L1: Discovery view (all skills, lightweight catalog entries)
function SkillCatalog:discover(filter)
    return self.registry:list(filter)
end

-- L1: Render catalog string within budget
function SkillCatalog:render_catalog(opts)
    opts = opts or {}
    local budget = opts.budget or self.budget_chars
    local entries = self:discover(opts.filter)
    local lines = {}
    local used = 0

    for _, entry in ipairs(entries) do
        local desc = entry.description or ""
        local line = string.format("- **%s**: %s", entry.name, desc)
        local cost = #line
        if used + cost > budget then break end
        table.insert(lines, line)
        used = used + cost
    end

    return table.concat(lines, "\n"), {
        total = #entries,
        shown = #lines,
        budget_used = used,
        budget_max = budget,
    }
end

-- Rough token estimate: ~4 chars per token
local function estimate_tokens(text)
    if not text then return 0 end
    return math.ceil(#text / 4)
end

-- Resolve the main file path for a skill
local function get_main_file(skill)
    local fmt = skill.source and skill.source.format
    local path = skill.source and skill.source.path or ""
    if fmt == "agent-skills" then
        return path .. "/SKILL.md"
    elseif fmt == "cursor-mdc" then
        -- Find *.mdc in the directory
        local result = orcs.glob("*.mdc", path)
        if result and result.ok and result.count > 0 then
            return result.files[1]
        end
        return path .. "/*.mdc"
    elseif fmt == "lua-dsl" then
        return path .. "/skill.lua"
    end
    return path
end

-- L2: Activate a skill (load body)
function SkillCatalog:activate(name)
    local skill = self.registry:get(name)
    if not skill then return nil, "skill not found: " .. name end

    if skill.state == "activated" then
        return skill
    end

    -- Load body if not yet loaded
    if not skill.body then
        local main_file = get_main_file(skill)
        local read_result = orcs.read(main_file)
        if not read_result or not read_result.ok then
            return nil, "failed to read: " .. main_file
        end

        if skill.source.format == "lua-dsl" then
            -- For Lua DSL, body is the prompt field
            local def = orcs.load_lua(read_result.content, main_file)
            if def and type(def) == "table" then
                skill.body = def.body or def.prompt or read_result.content
            else
                skill.body = read_result.content
            end
        else
            local parsed = orcs.parse_frontmatter_str(read_result.content)
            if parsed then
                skill.body = parsed.body
            else
                skill.body = read_result.content
            end
        end
        skill.token_estimate = estimate_tokens(skill.body)
    end

    skill.state = "activated"
    skill.activated_at = os.time and os.time() or 0
    self.activated[name] = true

    return skill
end

-- L2: Deactivate a skill (release body memory)
function SkillCatalog:deactivate(name)
    local skill = self.registry:get(name)
    if not skill then return false, "skill not found: " .. name end
    skill.body = nil
    skill.state = "discovered"
    skill.activated_at = nil
    skill.token_estimate = nil
    self.activated[name] = nil
    return true
end

-- L3: Load resources (scripts/, references/, assets/)
function SkillCatalog:load_resources(name, resource_type)
    local skill = self.registry:get(name)
    if not skill then return nil, "skill not found: " .. name end

    local resource_dir = (skill.source.path or "") .. "/" .. resource_type
    local ok, files = pcall(orcs.scan_dir, {
        path = resource_dir,
        recursive = false,
    })

    if not ok or not files or #files == 0 then
        return nil, "no resources found in: " .. resource_type
    end

    local resources = {}
    for _, f in ipairs(files) do
        if not f.is_dir then
            local read_result = orcs.read(f.path)
            if read_result and read_result.ok then
                resources[f.relative] = read_result.content
            end
        end
    end

    -- Cache on skill
    skill[resource_type] = resources
    return resources
end

-- List currently activated skills
function SkillCatalog:active_skills()
    local result = {}
    for name in pairs(self.activated) do
        local skill = self.registry:get(name)
        if skill then table.insert(result, skill) end
    end
    return result
end

-- Count of activated skills
function SkillCatalog:active_count()
    local n = 0
    for _ in pairs(self.activated) do n = n + 1 end
    return n
end

return SkillCatalog

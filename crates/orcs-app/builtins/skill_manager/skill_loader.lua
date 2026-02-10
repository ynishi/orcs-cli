-- skill_loader.lua
-- Loads skills from various formats (Agent Skills, Cursor MDC, Lua DSL).

local SkillLoader = {}

-- Main file per format
local FORMAT_MAIN_FILES = {
    ["agent-skills"] = "SKILL.md",
}

-- Helper: check if a file exists via orcs.read (returns nil on error)
local function file_exists(path)
    local result = orcs.read(path)
    return result and result.ok
end

-- Load skills from a directory (L1 Discovery)
function SkillLoader.load_dir(path, opts, format_adapter)
    opts = opts or {}

    local ok, entries = pcall(orcs.scan_dir, {
        path = path,
        recursive = true,
        exclude = opts.exclude or { ".git", "node_modules", ".DS_Store" },
        include = opts.include,
        max_depth = opts.max_depth or 3,
    })
    if not ok or not entries or #entries == 0 then
        return {}, {}
    end

    -- Detect skill directories: dirs containing SKILL.md, *.mdc, or skill.lua
    local skill_dirs = SkillLoader.detect_skill_dirs(entries, path)

    local skills = {}
    local errors = {}
    for _, dir in ipairs(skill_dirs) do
        local skill, err = SkillLoader.load_single(dir.path, dir.format, format_adapter)
        if skill then
            table.insert(skills, skill)
        else
            table.insert(errors, { path = dir.path, error = err })
        end
    end

    return skills, errors
end

-- Detect skill directories from scan results
function SkillLoader.detect_skill_dirs(entries, base_path)
    local dirs = {}
    local seen = {}

    for _, entry in ipairs(entries) do
        if not entry.is_dir then
            local name = entry.relative:match("([^/]+)$") or ""
            local dir = entry.path:match("(.+)/[^/]+$") or base_path

            if not seen[dir] then
                local format = nil
                if name == "SKILL.md" then
                    format = "agent-skills"
                elseif name:match("%.mdc$") then
                    format = "cursor-mdc"
                elseif name == "skill.lua" then
                    format = "lua-dsl"
                end

                if format then
                    seen[dir] = true
                    table.insert(dirs, { path = dir, format = format })
                end
            end
        end
    end

    return dirs
end

-- Load a single skill from a path
function SkillLoader.load_single(skill_path, format, format_adapter)
    format = format or SkillLoader.detect_format(skill_path)
    if not format then
        return nil, "unknown format at: " .. skill_path
    end

    local adapter = format_adapter.get(format)
    if not adapter then
        return nil, "no adapter for format: " .. format
    end

    -- Lua DSL: evaluate skill.lua
    if format == "lua-dsl" then
        return SkillLoader.load_lua_dsl(skill_path, adapter)
    end

    -- Markdown-based formats (agent-skills, cursor-mdc)
    local main_file = FORMAT_MAIN_FILES[format]
    if not main_file then
        -- For cursor-mdc, find the .mdc file
        if format == "cursor-mdc" then
            local result = orcs.glob("*.mdc", skill_path)
            if result and result.ok and result.count > 0 then
                main_file = result.files[1]:match("([^/]+)$")
            end
        end
        if not main_file then
            return nil, "cannot determine main file for format: " .. format
        end
    end

    local full_path = skill_path .. "/" .. main_file
    local read_result = orcs.read(full_path)
    if not read_result or not read_result.ok then
        return nil, "main file not found: " .. full_path
    end

    local parsed = orcs.parse_frontmatter_str(read_result.content)
    if not parsed then
        return nil, "frontmatter parse failed for: " .. full_path
    end

    local skill = adapter.to_internal(parsed, skill_path)
    -- L1 only: clear body for lazy loading
    skill.body = nil
    skill.state = "discovered"

    return skill
end

-- Load a Lua DSL skill
function SkillLoader.load_lua_dsl(skill_path, adapter)
    local lua_file = skill_path .. "/skill.lua"
    local read_result = orcs.read(lua_file)
    if not read_result or not read_result.ok then
        return nil, "skill.lua not found at: " .. skill_path
    end

    local def = orcs.load_lua(read_result.content, lua_file)
    if not def then
        return nil, "lua-dsl eval failed for: " .. lua_file
    end
    if type(def) ~= "table" then
        return nil, "skill.lua must return a table, got: " .. type(def)
    end

    local skill = adapter.to_internal(def, skill_path)
    skill.body = nil
    skill.state = "discovered"

    return skill
end

-- Auto-detect skill format from directory contents
function SkillLoader.detect_format(path)
    if file_exists(path .. "/SKILL.md") then
        return "agent-skills"
    end
    local mdc = orcs.glob("*.mdc", path)
    if mdc and mdc.ok and mdc.count > 0 then
        return "cursor-mdc"
    end
    if file_exists(path .. "/skill.lua") then
        return "lua-dsl"
    end
    return nil
end

-- Load a single file directly (for standalone .md or .mdc files)
function SkillLoader.load_file(file_path, format_adapter)
    local read_result = orcs.read(file_path)
    if not read_result or not read_result.ok then
        return nil, "file not found: " .. file_path
    end

    -- Determine format from extension
    local format
    if file_path:match("%.mdc$") then
        format = "cursor-mdc"
    elseif file_path:match("%.md$") then
        format = "agent-skills"
    elseif file_path:match("%.lua$") then
        format = "lua-dsl"
    else
        return nil, "unknown file format: " .. file_path
    end

    if format == "lua-dsl" then
        local adapter = format_adapter.get(format)
        if not adapter then return nil, "no adapter for lua-dsl" end
        local def = orcs.load_lua(read_result.content, file_path)
        if not def or type(def) ~= "table" then
            return nil, "lua-dsl eval failed"
        end
        local skill = adapter.to_internal(def, file_path)
        skill.state = "discovered"
        return skill
    end

    local parsed = orcs.parse_frontmatter_str(read_result.content)
    if not parsed then
        return nil, "frontmatter parse failed"
    end

    local adapter = format_adapter.get(format)
    if not adapter then
        return nil, "no adapter for: " .. format
    end

    local skill = adapter.to_internal(parsed, file_path)
    skill.state = "discovered"
    return skill
end

return SkillLoader

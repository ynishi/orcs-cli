-- format_adapter.lua
-- Converts various skill file formats to a common internal model.

local FormatAdapter = {
    adapters = {},
}

function FormatAdapter.register(format_name, adapter)
    FormatAdapter.adapters[format_name] = adapter
end

function FormatAdapter.get(format_name)
    return FormatAdapter.adapters[format_name]
end

function FormatAdapter.supported_formats()
    local names = {}
    for name in pairs(FormatAdapter.adapters) do
        table.insert(names, name)
    end
    table.sort(names)
    return names
end

-- Helper: extract directory name from path
local function dir_name(path)
    return path:match("([^/]+)/?$") or path
end

-- Helper: extract first paragraph from markdown body
local function extract_first_paragraph(body)
    if not body then return "" end
    -- Skip leading headings and blank lines
    local text = body:gsub("^#[^\n]*\n*", "")
    local para = text:match("^%s*(.-)%s*\n%s*\n")
    return para or text:sub(1, 200)
end

-- Helper: extract file stem (name without extension)
local function file_stem(path)
    local name = path:match("([^/]+)$") or path
    return name:match("(.+)%..+$") or name
end

-- === Agent Skills Standard (SKILL.md + YAML frontmatter) ===
FormatAdapter.register("agent-skills", {
    to_internal = function(parsed, skill_path)
        local fm = parsed.frontmatter or {}
        return {
            name = fm.name or dir_name(skill_path),
            description = fm.description or extract_first_paragraph(parsed.body),
            source = { format = "agent-skills", path = skill_path },
            body = parsed.body,
            frontmatter = fm,
            metadata = {
                author = fm.author,
                version = fm.version,
                tags = fm.tags or {},
                categories = fm.categories or {},
            },
            state = "discovered",
        }
    end,
})

-- === Cursor .mdc ===
FormatAdapter.register("cursor-mdc", {
    to_internal = function(parsed, skill_path)
        local fm = parsed.frontmatter or {}
        return {
            name = file_stem(skill_path),
            description = fm.description or "",
            source = { format = "cursor-mdc", path = skill_path },
            body = parsed.body,
            frontmatter = {
                ["user-invocable"] = not fm.alwaysApply,
                ["disable-model-invocation"] = fm.alwaysApply or false,
            },
            metadata = {
                globs = fm.globs,
                tags = {},
                categories = {},
            },
            state = "discovered",
        }
    end,
})

-- === Lua DSL (skill.lua returning a table) ===
FormatAdapter.register("lua-dsl", {
    to_internal = function(def, skill_path)
        -- def is the table returned by skill.lua
        return {
            name = def.name,
            description = def.description or "",
            source = { format = "lua-dsl", path = skill_path },
            body = def.body or def.prompt or "",
            frontmatter = def.frontmatter or {},
            metadata = {
                author = def.author,
                version = def.version,
                tags = def.tags or {},
                categories = def.categories or {},
            },
            state = "discovered",
        }
    end,
})

return FormatAdapter

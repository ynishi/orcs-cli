-- foundation_manager.lua
-- Foundation Manager: reads and serves Agent.md foundation segments.
--
-- Reads `.orcs/agent.md` (project-level) and `~/.orcs/agent.md` (global).
-- Parses `# Section` headings into named segments (system, task, guard).
-- Serves segments via RPC for prompt assembly by agent_mgr.
--
-- Search order (later overrides earlier):
--   1. ~/.orcs/agent.md   (global defaults)
--   2. .orcs/agent.md     (project-specific)
--
-- Agent.md format:
--   ---
--   name: "My Agent"
--   ---
--   # System
--   Core identity and rules.
--   # Task
--   Project-specific guidelines.
--   # Guard
--   Output constraints and guardrails.
--
-- Operations:
--   get_segment  -> { name: "system" } -> single segment text
--   get_all      -> all segments as { system, task, guard, ... }
--   reload       -> re-read agent.md files
--   status       -> component status summary
--   profile_apply -> accept overrides from profile system

-- === Module State ===

local segments = {}            -- name -> content string
local metadata = {}            -- frontmatter key-values
local loaded_files = {}        -- list of loaded file paths
local component_settings = {}  -- from [components.settings.foundation_manager]

-- === Helpers ===

--- Count keys in a table (non-array).
local function count_keys(t)
    local n = 0
    for _ in pairs(t) do n = n + 1 end
    return n
end

-- === Parsing ===

--- Parse an agent.md file into segments keyed by lowercase heading name.
--- Top-level headings (`# Name`) delimit segments.
--- Sub-headings (`##`, `###`, ...) are included in the parent segment body.
--- YAML frontmatter (`---`...`---`) is parsed for metadata.
---
--- Returns (segments_table, metadata_table)
local function parse_agent_md(content)
    local result = {}
    local meta = {}
    local body = content

    -- Normalize CRLF to LF for cross-platform compatibility
    body = body:gsub("\r\n", "\n")

    -- Strip YAML frontmatter if present
    if body:sub(1, 4) == "---\n" then
        local close = body:find("\n%-%-%-\n", 5)
        if close then
            local fm = body:sub(5, close - 1)
            for line in fm:gmatch("[^\n]+") do
                local key, val = line:match("^(%S+):%s*(.+)$")
                if key and val then
                    -- Strip surrounding quotes
                    meta[key] = val:gsub('^"(.*)"$', '%1')
                end
            end
            body = body:sub(close + 5)
        end
    end

    -- Parse top-level # sections
    local current_section = nil
    local current_lines = {}

    for line in (body .. "\n"):gmatch("(.-)\n") do
        -- Only match top-level headings (single #)
        local heading = line:match("^#%s+(.+)$")
        -- Exclude ## and deeper (they belong to the current section)
        local is_sub = line:match("^##")

        if heading and not is_sub then
            -- Save previous section
            if current_section then
                local text = table.concat(current_lines, "\n")
                text = text:match("^%s*(.-)%s*$") or ""
                if text ~= "" then
                    result[current_section:lower()] = text
                end
            end
            current_section = heading
            current_lines = {}
        else
            if current_section then
                current_lines[#current_lines + 1] = line
            end
        end
    end

    -- Save last section
    if current_section then
        local text = table.concat(current_lines, "\n")
        text = text:match("^%s*(.-)%s*$") or ""
        if text ~= "" then
            result[current_section:lower()] = text
        end
    end

    return result, meta
end

--- Merge two segment tables. Overlay values override base.
local function merge_segments(base, overlay)
    local merged = {}
    for k, v in pairs(base) do
        merged[k] = v
    end
    for k, v in pairs(overlay) do
        merged[k] = v
    end
    return merged
end

-- === File Loading ===

--- Load and parse agent.md from search paths.
--- Global (~/.orcs/agent.md) first, then project (.orcs/agent.md) overrides.
local function load_foundation()
    local all_segments = {}
    local all_meta = {}
    local files = {}

    -- 1. Global agent.md
    local home = os.getenv("HOME")
    if home then
        local global_path = home .. "/.orcs/agent.md"
        local result = orcs.read(global_path)
        if result and result.ok and result.content and result.content ~= "" then
            local segs, meta = parse_agent_md(result.content)
            all_segments = merge_segments(all_segments, segs)
            all_meta = merge_segments(all_meta, meta)
            files[#files + 1] = global_path
            orcs.log("debug", string.format(
                "foundation: loaded global agent.md (%d segments)", count_keys(segs)
            ))
        end
    end

    -- 2. Project agent.md (overrides global)
    if not orcs.pwd or orcs.pwd == "" then
        orcs.log("warn", "foundation: orcs.pwd is not set, skipping project agent.md")
        return all_segments, all_meta, files
    end
    local project_path = orcs.pwd .. "/.orcs/agent.md"
    local result = orcs.read(project_path)
    if result and result.ok and result.content and result.content ~= "" then
        local segs, meta = parse_agent_md(result.content)
        all_segments = merge_segments(all_segments, segs)
        all_meta = merge_segments(all_meta, meta)
        files[#files + 1] = project_path
        orcs.log("debug", string.format(
            "foundation: loaded project agent.md (%d segments)", count_keys(segs)
        ))
    end

    return all_segments, all_meta, files
end

-- === Request Handlers ===

--- Get a single segment by name.
--- Payload: { name = "system" }
local function handle_get_segment(payload)
    if not payload or not payload.name then
        return { success = false, error = "payload.name is required" }
    end

    local name = payload.name:lower()
    local content = segments[name]

    if content then
        return { success = true, data = { name = name, content = content } }
    else
        return {
            success = true,
            data = { name = name, content = "" },
        }
    end
end

--- Get all segments.
local function handle_get_all(_payload)
    return {
        success = true,
        data = segments,
        metadata = metadata,
        count = count_keys(segments),
    }
end

--- Reload agent.md files from disk.
local function handle_reload(_payload)
    segments, metadata, loaded_files = load_foundation()
    return {
        success = true,
        data = {
            segments = count_keys(segments),
            files = loaded_files,
        },
    }
end

--- Component status summary.
local function handle_status(_payload)
    local segment_names = {}
    for k in pairs(segments) do
        segment_names[#segment_names + 1] = k
    end
    table.sort(segment_names)

    return {
        success = true,
        data = {
            segments = segment_names,
            segment_count = #segment_names,
            files = loaded_files,
            metadata = metadata,
        },
    }
end

--- Accept overrides from the profile system.
--- Payload: { profile = "name", settings = { system = "...", task = "...", ... } }
local function handle_profile_apply(payload)
    if not payload or not payload.settings then
        return { success = true, data = { applied = false, reason = "no settings" } }
    end

    local settings = payload.settings
    local applied = {}

    -- Override individual segments if provided
    for key, value in pairs(settings) do
        if type(value) == "string" and value ~= "" then
            segments[key:lower()] = value
            applied[#applied + 1] = key:lower()
        end
    end

    if #applied > 0 then
        orcs.log("info", string.format(
            "foundation: profile '%s' applied overrides: %s",
            payload.profile or "?",
            table.concat(applied, ", ")
        ))
    end

    return {
        success = true,
        data = {
            applied = true,
            overridden_segments = applied,
            profile = payload.profile,
        },
    }
end

-- === Handler Dispatch ===

local handlers = {
    get_segment   = handle_get_segment,
    get_all       = handle_get_all,
    reload        = handle_reload,
    status        = handle_status,
    profile_apply = handle_profile_apply,
}

-- === Component Definition ===

return {
    id = "foundation_manager",
    namespace = "foundation",
    subscriptions = {},
    elevated = true,

    init = function(cfg)
        if cfg and type(cfg) == "table" then
            component_settings = cfg
            orcs.log("debug", "foundation_manager: config received: " .. orcs.json_encode(cfg))
        end

        -- Load agent.md files
        segments, metadata, loaded_files = load_foundation()

        local seg_names = {}
        for k in pairs(segments) do
            seg_names[#seg_names + 1] = k
        end
        table.sort(seg_names)

        orcs.log("info", string.format(
            "foundation_manager initialized (files: %d, segments: [%s])",
            #loaded_files,
            table.concat(seg_names, ", ")
        ))
    end,

    shutdown = function()
        segments = {}
        metadata = {}
        loaded_files = {}
        orcs.log("info", "foundation_manager shutdown")
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
        return {
            segments = segments,
            metadata = metadata,
            loaded_files = loaded_files,
        }
    end,

    restore = function(state)
        if not state then return end
        segments = state.segments or {}
        metadata = state.metadata or {}
        loaded_files = state.loaded_files or {}
        orcs.log("info", string.format(
            "foundation_manager restored (%d segments)", count_keys(segments)
        ))
    end,
}

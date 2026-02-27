-- mcp_manager.lua
-- MCPManagerComponent: Server listing / Tool discovery / Tool invocation
-- for MCP (Model Context Protocol) servers configured in config.toml.
--
-- The Rust side (orcs-mcp crate) handles actual MCP connections.
-- This component provides the RPC interface for LLM and user interaction.
--
-- ## Config: [mcp.servers.<name>]
--
-- MCP servers are configured at the top level [mcp] section, NOT under
-- [components.settings.mcp_manager]. Connection management is done by
-- the Rust engine before components are spawned.
--
-- ## Config: [components.settings.mcp_manager]
--
-- | Key                        | Type   | Default  | Description                        |
-- |----------------------------|--------|----------|------------------------------------|
-- | recommend_llm_provider     | string | "ollama" | LLM provider for tool recommend    |
-- | recommend_llm_model        | string | default  | Model name for recommend           |
-- | recommend_llm_timeout      | number | 120      | Timeout for recommend (seconds)    |

-- Module-level state
local component_settings = {}
local initialized = false

-- === Request Handlers ===

-- List connected MCP servers
local function handle_list(_payload)
    local resp = orcs.mcp_servers()
    if not resp or not resp.ok then
        return { success = false, error = resp and resp.error or "mcp_servers() failed" }
    end
    return { success = true, data = resp.servers, count = #resp.servers }
end

-- Get info about a specific server (tools provided by it)
local function handle_get(payload)
    if not payload or not payload.name then
        return { success = false, error = "payload.name is required" }
    end
    local resp = orcs.mcp_tools(payload.name)
    if not resp or not resp.ok then
        return { success = false, error = resp and resp.error or "mcp_tools() failed" }
    end
    return {
        success = true,
        data = {
            server = payload.name,
            tools = resp.tools,
            tool_count = #resp.tools,
        },
    }
end

-- List MCP tools (optionally filtered by server)
local function handle_tools(payload)
    local server = payload and payload.server
    local resp = orcs.mcp_tools(server)
    if not resp or not resp.ok then
        return { success = false, error = resp and resp.error or "mcp_tools() failed" }
    end
    return { success = true, data = resp.tools, count = #resp.tools }
end

-- Search tools by keyword
local function handle_search(payload)
    if not payload or not payload.query then
        return { success = false, error = "payload.query is required" }
    end
    local query = payload.query:lower()
    local resp = orcs.mcp_tools()
    if not resp or not resp.ok then
        return { success = false, error = resp and resp.error or "mcp_tools() failed" }
    end

    local matches = {}
    for _, tool in ipairs(resp.tools) do
        local name_lower = (tool.name or ""):lower()
        local desc_lower = (tool.description or ""):lower()
        if name_lower:find(query, 1, true) or desc_lower:find(query, 1, true) then
            table.insert(matches, tool)
        end
    end
    return { success = true, data = matches, count = #matches }
end

-- Run (call) an MCP tool
-- payload: { server, tool, args }
-- or: { name = "mcp:server:tool", args }
local function handle_run(payload)
    if not payload then
        return { success = false, error = "payload is required" }
    end

    local server, tool, args
    if payload.name then
        -- Parse "mcp:server:tool" format
        local s, t = payload.name:match("^mcp:([^:]+):(.+)$")
        if not s then
            -- Try "server:tool" format
            s, t = payload.name:match("^([^:]+):(.+)$")
        end
        if not s then
            return { success = false, error = "invalid tool name format, expected 'server:tool' or 'mcp:server:tool'" }
        end
        server = s
        tool = t
        args = payload.args or {}
    else
        server = payload.server
        tool = payload.tool
        args = payload.args or {}
    end

    if not server or not tool then
        return { success = false, error = "payload.server and payload.tool are required (or payload.name)" }
    end

    local resp = orcs.mcp_call(server, tool, args)
    if not resp then
        return { success = false, error = "mcp_call returned nil" }
    end

    return {
        success = resp.ok == true,
        data = resp.content and { content = resp.content } or nil,
        error = resp.error,
    }
end

-- Status: overall MCP manager state
local function handle_status(_payload)
    local servers_resp = orcs.mcp_servers()
    local tools_resp = orcs.mcp_tools()

    local server_count = 0
    if servers_resp and servers_resp.ok then
        server_count = #servers_resp.servers
    end

    local tool_count = 0
    if tools_resp and tools_resp.ok then
        tool_count = #tools_resp.tools
    end

    return {
        success = true,
        data = {
            servers = server_count,
            tools = tool_count,
            initialized = initialized,
        },
    }
end

-- Recommend tools for an intent using LLM
local function handle_recommend(payload)
    if not payload or not payload.intent then
        return { success = false, error = "payload.intent is required" }
    end

    local intent = payload.intent
    local limit = payload.limit or 5

    -- Gather all MCP tools
    local resp = orcs.mcp_tools()
    if not resp or not resp.ok or #resp.tools == 0 then
        return { success = true, data = {}, count = 0 }
    end

    -- Build tool catalog for LLM prompt
    local tool_lines = {}
    for i, t in ipairs(resp.tools) do
        local desc = t.description or ""
        if #desc > 300 then
            desc = desc:sub(1, 300) .. "..."
        end
        tool_lines[i] = string.format("%d. %s [%s] — %s", i, t.tool, t.server, desc)
    end
    local tool_list = table.concat(tool_lines, "\n")

    local prompt_parts = {
        "You are a tool recommender. Given a user's intent and available MCP tools, select the most relevant tools.",
        "",
        "## User Intent",
        intent,
        "",
        "## Available MCP Tools (" .. #resp.tools .. " total)",
        tool_list,
        "",
        "## Instructions",
        "- Select 0-" .. limit .. " tools that are directly relevant to the user's intent.",
        "- If no tool is relevant, return an empty list.",
        '- Return ONLY a JSON array of objects: [{"server":"X","tool":"Y"}, ...]',
        "- No explanation needed, just the JSON array.",
    }
    local prompt = table.concat(prompt_parts, "\n")

    -- Build LLM opts from component config
    local llm_opts = {}
    local llm_key_map = {
        recommend_llm_provider    = "provider",
        recommend_llm_model       = "model",
        recommend_llm_base_url    = "base_url",
        recommend_llm_api_key     = "api_key",
        recommend_llm_temperature = "temperature",
        recommend_llm_max_tokens  = "max_tokens",
        recommend_llm_timeout     = "timeout",
    }
    for src_key, dst_key in pairs(llm_key_map) do
        if component_settings[src_key] ~= nil then
            llm_opts[dst_key] = component_settings[src_key]
        end
    end
    if payload.model then
        llm_opts.model = payload.model
    end

    local llm_resp = orcs.llm(prompt, llm_opts)
    if not llm_resp or not llm_resp.ok then
        -- Fallback to keyword search
        orcs.log("warn", "MCPManager recommend: LLM failed, falling back to keyword search")
        local search_result = handle_search({ query = intent:sub(1, 50) })
        if search_result.success then
            return { success = true, data = search_result.data, count = search_result.count, fallback = true }
        end
        return { success = true, data = {}, count = 0, fallback = true }
    end

    -- Parse LLM response
    local content = llm_resp.content or ""
    local stripped = content:gsub("```json%s*", ""):gsub("```%s*", "")
    local json_str = stripped:match("%[.-%]")
    if not json_str then
        orcs.log("warn", "MCPManager recommend: could not parse LLM response, fallback")
        return { success = true, data = {}, count = 0, fallback = true }
    end

    local ok, parsed = pcall(orcs.json_parse, json_str)
    if not ok or type(parsed) ~= "table" then
        orcs.log("warn", "MCPManager recommend: JSON parse failed, fallback")
        return { success = true, data = {}, count = 0, fallback = true }
    end

    -- Resolve to tool entries
    local recommended = {}
    for _, entry in ipairs(parsed) do
        if #recommended >= limit then break end
        if type(entry) == "table" and entry.server and entry.tool then
            table.insert(recommended, {
                server = entry.server,
                tool = entry.tool,
                name = string.format("mcp:%s:%s", entry.server, entry.tool),
            })
        end
    end

    orcs.log("info", string.format(
        "MCPManager recommend: intent=%s → %d tools (of %d total)",
        intent:sub(1, 50), #recommended, #resp.tools
    ))

    return { success = true, data = recommended, count = #recommended }
end

-- Tool descriptions (formatted text for LLM context)
local function handle_tool_descriptions(payload)
    local server = payload and payload.server
    local resp = orcs.mcp_tools(server)
    if not resp or not resp.ok then
        return { success = true, data = { text = "" } }
    end

    local lines = {}
    for _, t in ipairs(resp.tools) do
        table.insert(lines, string.format("- %s [%s]: %s", t.tool, t.server, t.description or ""))
    end
    return { success = true, data = { text = table.concat(lines, "\n"), count = #resp.tools } }
end

-- === Handler dispatch table ===
local handlers = {
    -- Server discovery
    list             = handle_list,
    get              = handle_get,
    -- Tool operations
    tools            = handle_tools,
    search           = handle_search,
    run              = handle_run,
    call_tool        = handle_run,       -- alias
    -- Selection
    recommend        = handle_recommend,
    tool_descriptions = handle_tool_descriptions,
    -- Meta
    status           = handle_status,
}

-- === Component Definition ===
return {
    id = "mcp_manager",
    namespace = "mcp",
    subscriptions = {},
    elevated = true,

    init = function(cfg)
        if cfg and type(cfg) == "table" then
            component_settings = cfg
            orcs.log("debug", "MCPManager: received config: " .. orcs.json_encode(cfg))
        end

        -- Check status from Rust-side MCP manager
        local status = handle_status()
        initialized = true

        orcs.log("info", string.format(
            "MCPManager initialized (servers: %d, tools: %d)",
            status.data.servers, status.data.tools
        ))
    end,

    shutdown = function()
        initialized = false
        orcs.log("info", "MCPManager shutdown")
    end,

    on_request = function(request)
        local handler = handlers[request.operation]
        if handler then
            return handler(request.payload)
        end
        return { success = false, error = "unknown operation: " .. tostring(request.operation) }
    end,

    on_signal = function(signal)
        if signal.kind == "Veto" then
            return "Abort"
        end
        return "Handled"
    end,

    snapshot = function()
        return { initialized = initialized }
    end,

    restore = function(state)
        if not state then return end
        initialized = state.initialized or false
        orcs.log("info", "MCPManager restored")
    end,
}

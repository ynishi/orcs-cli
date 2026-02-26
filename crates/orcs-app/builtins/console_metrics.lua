-- console_metrics.lua
-- Console Metrics component: provides real-time workspace context for prompt injection.
--
-- Gathers environment metadata (git state, working directory, timestamp, etc.)
-- and serves it via RPC for prompt assembly by agent_mgr.
--
-- Metrics are cached and refreshed on each get_all call to ensure freshness
-- without redundant subprocess invocations within a single prompt cycle.
--
-- Operations:
--   get_all      -> all metrics as { cwd, project_name, git, timestamp, agents }
--   get_metric   -> { name: "git" } -> single metric
--   refresh      -> force refresh all metrics
--   status       -> component status summary
--
-- External dependencies:
--   builtin::agent_mgr (list_agents) -> registered/spawned agent status

-- === Module State ===

local metrics = {}             -- name -> value (cached)
local component_settings = {}  -- from [components.settings.console_metrics]

-- === Helpers ===

--- Extract basename from a path string.
local function basename(path)
    return path:match("([^/]+)$") or path
end

--- Format metrics as a prompt-friendly string.
local function format_for_prompt(m)
    local parts = {}

    parts[#parts + 1] = "[Console Metrics]"

    if m.project_name and m.project_name ~= "" then
        parts[#parts + 1] = "Project: " .. m.project_name
    end

    if m.cwd and m.cwd ~= "" then
        parts[#parts + 1] = "Working Directory: " .. m.cwd
    end

    if m.git then
        local git = m.git
        if git.ok then
            local git_line = "Git: " .. (git.branch or "?")
            if git.commit_short and git.commit_short ~= "" then
                git_line = git_line .. " (" .. git.commit_short .. ")"
            end
            if git.dirty then
                git_line = git_line .. " [dirty]"
            end
            parts[#parts + 1] = git_line
        end
    end

    if m.agents then
        local a = m.agents
        -- Concierge info
        local concierge = a.concierge_fqn or "none"
        local agent_line = "Agents: concierge=" .. concierge
        -- Provider/model info (from agent_mgr config, available for builtin concierge)
        if a.llm_provider then
            agent_line = agent_line .. string.format(
                ", llm=%s/%s", a.llm_provider, a.llm_model or "default"
            )
        end
        -- Delegate worker
        agent_line = agent_line .. ", delegate=" .. (a.delegate_fqn and "ok" or "off")
        -- Delegation stats
        if a.delegation_count and a.delegation_count > 0 then
            agent_line = agent_line .. string.format(
                ", delegations=%d (completed=%d)",
                a.delegation_count, a.delegation_completed or 0
            )
        end
        parts[#parts + 1] = agent_line
    end

    if m.timestamp and m.timestamp ~= "" then
        parts[#parts + 1] = "Time: " .. m.timestamp
    end

    -- Agent status (from agent_mgr list_agents RPC)
    if m.agents then
        local a = m.agents
        local agent_parts = {}
        if a.registered and #a.registered > 0 then
            local names = {}
            for _, reg in ipairs(a.registered) do
                local label = reg.name
                if reg.spawned then label = label .. " (running)" end
                names[#names + 1] = label
            end
            agent_parts[#agent_parts + 1] = "Available: " .. table.concat(names, ", ")
        end
        if a.spawned and #a.spawned > 0 then
            local persistent = {}
            for _, s in ipairs(a.spawned) do
                if s.persistent then
                    persistent[#persistent + 1] = s.name
                end
            end
            if #persistent > 0 then
                agent_parts[#agent_parts + 1] = "Persistent: " .. table.concat(persistent, ", ")
            end
        end
        if #agent_parts > 0 then
            parts[#parts + 1] = "Agents: " .. table.concat(agent_parts, " | ")
        end
    end

    return table.concat(parts, "\n")
end

-- === Metric Collection ===

--- Collect base metrics (local only, no cross-component RPC).
--- Safe to call during init() before other components are ready.
local function collect_base_metrics()
    local m = {}

    -- Working directory
    m.cwd = orcs.pwd or ""

    -- Project name (basename of cwd)
    m.project_name = basename(m.cwd)

    -- Git info (via Rust API, no permission needed)
    if orcs.git_info then
        m.git = orcs.git_info()
    else
        m.git = { ok = false }
    end

    -- Timestamp
    m.timestamp = os.date("%Y-%m-%d %H:%M:%S")

    -- Agent status (query agent_mgr for registered/spawned agents).
    -- Non-critical: if agent_mgr is slow or unavailable, skip gracefully.
    local agent_ok, agent_resp = pcall(orcs.request, "builtin::agent_mgr", "list_agents", {})
    if agent_ok and agent_resp and agent_resp.success and agent_resp.data then
        m.agents = agent_resp.data
    end

    return m
end

--- Collect all metrics including cross-component agent info.
--- Only call after all components are initialized (i.e., from RPC handlers).
local function collect_metrics()
    local m = collect_base_metrics()

    -- Agent info (via RPC to agent_mgr)
    local agent_resp = orcs.request("builtin::agent_mgr", "list_active", {})
    if agent_resp and agent_resp.success and agent_resp.data then
        m.agents = agent_resp.data
    end

    return m
end

-- === Request Handlers ===

--- Get all metrics (refreshes cache).
local function handle_get_all(_payload)
    metrics = collect_metrics()
    return {
        success = true,
        data = metrics,
        formatted = format_for_prompt(metrics),
    }
end

--- Get a single metric by name.
--- Payload: { name = "git" }
local function handle_get_metric(payload)
    if not payload or not payload.name then
        return { success = false, error = "payload.name is required" }
    end

    -- Refresh if cache is empty
    if not next(metrics) then
        metrics = collect_metrics()
    end

    local name = payload.name:lower()
    local value = metrics[name]

    if value ~= nil then
        return { success = true, data = { name = name, value = value } }
    else
        return { success = true, data = { name = name, value = nil } }
    end
end

--- Force refresh all metrics.
local function handle_refresh(_payload)
    metrics = collect_metrics()
    return {
        success = true,
        data = { refreshed = true, metric_count = 0 },
    }
end

--- Component status summary.
local function handle_status(_payload)
    local metric_names = {}
    for k in pairs(metrics) do
        metric_names[#metric_names + 1] = k
    end
    table.sort(metric_names)

    return {
        success = true,
        data = {
            metrics = metric_names,
            metric_count = #metric_names,
            cached = next(metrics) ~= nil,
        },
    }
end

-- === Handler Dispatch ===

local handlers = {
    get_all    = handle_get_all,
    get_metric = handle_get_metric,
    refresh    = handle_refresh,
    status     = handle_status,
}

-- === Component Definition ===

return {
    id = "console_metrics",
    namespace = "metrics",
    subscriptions = {},
    elevated = false,

    init = function(cfg)
        if cfg and type(cfg) == "table" then
            component_settings = cfg
            orcs.log("debug", "console_metrics: config received: " .. orcs.json_encode(cfg))
        end

        -- Initial collection (base metrics only, no cross-component RPC
        -- to avoid blocking on components that may still be initializing)
        metrics = collect_base_metrics()

        local git_status = "no repo"
        if metrics.git and metrics.git.ok then
            git_status = (metrics.git.branch or "?")
            if metrics.git.dirty then
                git_status = git_status .. " [dirty]"
            end
        end

        orcs.log("info", string.format(
            "console_metrics initialized (project=%s, git=%s)",
            metrics.project_name or "?",
            git_status
        ))
    end,

    shutdown = function()
        metrics = {}
        orcs.log("info", "console_metrics shutdown")
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
            metrics = metrics,
        }
    end,

    restore = function(state)
        if not state then return end
        metrics = state.metrics or {}
        local count = 0
        for _ in pairs(metrics) do count = count + 1 end
        orcs.log("info", string.format(
            "console_metrics restored (%d metrics)", count
        ))
    end,
}

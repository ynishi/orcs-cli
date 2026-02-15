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
--   get_all      -> all metrics as { cwd, project_name, git, timestamp }
--   get_metric   -> { name: "git" } -> single metric
--   refresh      -> force refresh all metrics
--   status       -> component status summary

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

    if m.timestamp and m.timestamp ~= "" then
        parts[#parts + 1] = "Time: " .. m.timestamp
    end

    return table.concat(parts, "\n")
end

-- === Metric Collection ===

--- Collect all metrics from available sources.
local function collect_metrics()
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

        -- Initial collection
        metrics = collect_metrics()

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
        orcs.log("info", string.format(
            "console_metrics restored (%d metrics)", #metrics
        ))
    end,
}

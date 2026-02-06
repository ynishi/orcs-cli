-- code_agent.lua
-- POC: Lua agent that uses orcs.llm() + orcs tools for coding tasks.
--
-- Flow:
--   1. Receives user message
--   2. Builds prompt with tool descriptions + project context
--   3. Calls orcs.llm() (claude -p)
--   4. Parses tool calls from response and executes them
--   5. Feeds results back for follow-up if needed
--
-- Usage: @code_agent <task description>

local MAX_TURNS = 5

--- Build system prompt with tool context.
local function build_system()
    local tools = orcs.tool_descriptions()
    return "You are a coding assistant working in: " .. orcs.pwd .. "\n\n"
        .. tools .. "\n"
        .. "When you need to use a tool, output EXACTLY this format on its own line:\n"
        .. "TOOL_CALL: <tool_name>(<args>)\n"
        .. "Example: TOOL_CALL: read(\"src/main.rs\")\n"
        .. "Example: TOOL_CALL: grep(\"TODO\", \"src\")\n"
        .. "Example: TOOL_CALL: write(\"out.txt\", \"content here\")\n\n"
        .. "After receiving tool results, continue your analysis.\n"
        .. "When done, output your final answer without any TOOL_CALL lines."
end

--- Parse TOOL_CALL lines from LLM response.
--- Returns: list of {tool, args_str}, remaining text
local function parse_tool_calls(text)
    local calls = {}
    local clean_lines = {}

    for line in text:gmatch("[^\n]+") do
        local tool, args = line:match("^TOOL_CALL:%s*(%w+)%((.+)%)%s*$")
        if tool then
            table.insert(calls, { tool = tool, args_str = args })
        else
            table.insert(clean_lines, line)
        end
    end

    return calls, table.concat(clean_lines, "\n")
end

--- Execute a single tool call and return result string.
local function execute_tool(call)
    local t, a = call.tool, call.args_str

    -- Parse quoted string arguments
    local function unquote(s)
        return s:match('^"(.*)"$') or s:match("^'(.*)'$") or s
    end

    if t == "read" then
        local path = unquote(a:match("^%s*(.-)%s*$"))
        local r = orcs.read(path)
        if r.ok then
            return "[read " .. path .. "] " .. r.size .. " bytes:\n" .. r.content:sub(1, 2000)
        else
            return "[read " .. path .. "] ERROR: " .. r.error
        end

    elseif t == "write" then
        local path, content = a:match('^%s*"(.-)"%s*,%s*"(.*)"')
        if not path then
            path, content = a:match("^%s*'(.-)'%s*,%s*'(.*)'")
        end
        if path and content then
            local r = orcs.write(path, content)
            if r.ok then
                return "[write " .. path .. "] OK: " .. r.bytes_written .. " bytes"
            else
                return "[write " .. path .. "] ERROR: " .. r.error
            end
        end
        return "[write] ERROR: could not parse arguments"

    elseif t == "grep" then
        local pattern, path = a:match('^%s*"(.-)"%s*,%s*"(.-)"')
        if not pattern then
            pattern, path = a:match("^%s*'(.-)'%s*,%s*'(.-)'")
        end
        if pattern and path then
            local r = orcs.grep(pattern, path)
            if r.ok then
                local lines = {}
                for i = 1, math.min(r.count, 20) do
                    table.insert(lines, r.matches[i].line_number .. ": " .. r.matches[i].line)
                end
                return "[grep " .. pattern .. " in " .. path .. "] " .. r.count .. " matches:\n" .. table.concat(lines, "\n")
            else
                return "[grep] ERROR: " .. r.error
            end
        end
        return "[grep] ERROR: could not parse arguments"

    elseif t == "glob" then
        local pattern = unquote(a:match("^%s*(.-)%s*$"))
        local r = orcs.glob(pattern)
        if r.ok then
            local list = {}
            for i = 1, math.min(r.count, 50) do
                table.insert(list, r.files[i])
            end
            return "[glob " .. pattern .. "] " .. r.count .. " files:\n" .. table.concat(list, "\n")
        else
            return "[glob] ERROR: " .. r.error
        end

    elseif t == "exec" then
        local cmd = unquote(a:match("^%s*(.-)%s*$"))
        local r = orcs.exec(cmd)
        if r.ok then
            return "[exec] OK:\n" .. r.stdout:sub(1, 2000)
        else
            return "[exec] ERROR (code " .. r.code .. "):\n" .. r.stderr:sub(1, 1000)
        end

    elseif t == "mkdir" then
        local path = unquote(a:match("^%s*(.-)%s*$"))
        local r = orcs.mkdir(path)
        return r.ok and ("[mkdir " .. path .. "] OK") or ("[mkdir] ERROR: " .. r.error)

    elseif t == "remove" then
        local path = unquote(a:match("^%s*(.-)%s*$"))
        local r = orcs.remove(path)
        return r.ok and ("[remove " .. path .. "] OK") or ("[remove] ERROR: " .. r.error)

    elseif t == "mv" then
        local src, dst = a:match('^%s*"(.-)"%s*,%s*"(.-)"')
        if src and dst then
            local r = orcs.mv(src, dst)
            return r.ok and ("[mv " .. src .. " -> " .. dst .. "] OK") or ("[mv] ERROR: " .. r.error)
        end
        return "[mv] ERROR: could not parse arguments"

    else
        return "[unknown tool: " .. t .. "]"
    end
end

return {
    id = "code_agent",
    namespace = "builtin",
    subscriptions = {"UserInput"},

    on_request = function(request)
        local message = request.payload
        if type(message) == "table" then
            message = message.message or message.content or ""
        end
        if type(message) ~= "string" or message == "" then
            orcs.output("[code_agent] Usage: @code_agent <task>")
            return { success = true }
        end

        orcs.log("info", "[code_agent] task: " .. message:sub(1, 80))

        local system = build_system()
        local conversation = system .. "\n\nUser: " .. message

        for turn = 1, MAX_TURNS do
            orcs.log("info", "[code_agent] turn " .. turn .. "/" .. MAX_TURNS)

            local resp = orcs.llm(conversation)
            if not resp.ok then
                orcs.output("[code_agent] LLM error: " .. (resp.error or "unknown"))
                return { success = false, error = resp.error }
            end

            local calls, text = parse_tool_calls(resp.content)

            if #calls == 0 then
                -- No tool calls = final answer
                orcs.output(resp.content)
                return { success = true, data = { turns = turn, response = resp.content } }
            end

            -- Execute tool calls
            local tool_results = {}
            for _, call in ipairs(calls) do
                orcs.log("info", "[code_agent] tool: " .. call.tool .. "(" .. call.args_str .. ")")
                local result = execute_tool(call)
                table.insert(tool_results, result)
            end

            -- Append results to conversation for next turn
            local results_text = table.concat(tool_results, "\n\n")
            conversation = conversation
                .. "\n\nAssistant: " .. text
                .. "\n\nTool Results:\n" .. results_text
                .. "\n\nContinue based on the tool results above."
        end

        orcs.output("[code_agent] Max turns reached (" .. MAX_TURNS .. ")")
        return { success = true, data = { turns = MAX_TURNS, truncated = true } }
    end,

    on_signal = function(signal)
        if signal.kind == "Veto" then
            return "Abort"
        end
        return "Handled"
    end,

    init = function()
        orcs.log("info", "code_agent initialized (pwd=" .. orcs.pwd .. ")")
        orcs.output("[code_agent] Ready. Use @code_agent <task> to start.")
    end,

    shutdown = function()
        orcs.log("info", "code_agent shutdown")
    end,
}

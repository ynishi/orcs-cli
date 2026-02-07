-- code_agent.lua
-- Lua agent that uses orcs.llm() + orcs.dispatch() for coding tasks.
--
-- Flow:
--   1. Receives user message
--   2. Builds prompt with tool schemas + project context
--   3. Calls orcs.llm()
--   4. Parses JSON tool calls from response and dispatches them
--   5. Feeds results back for follow-up if needed
--
-- Tool calls are JSON objects on their own line:
--   TOOL_CALL: {"tool":"read","args":{"path":"src/main.rs"}}
--
-- Usage: @code_agent <task description>

local MAX_TURNS = 5

--- Build tool description from schemas for LLM prompt.
local function build_tool_section()
    local schemas = orcs.tool_schemas()
    local lines = { "Available tools:" }

    for _, schema in ipairs(schemas) do
        local arg_parts = {}
        for _, arg in ipairs(schema.args) do
            local marker = arg.required and "" or "?"
            table.insert(arg_parts, arg.name .. marker .. ": " .. arg.type)
        end
        table.insert(lines, "  " .. schema.name .. "(" .. table.concat(arg_parts, ", ") .. ")")
        table.insert(lines, "    " .. schema.description)
    end

    return table.concat(lines, "\n")
end

--- Build system prompt with tool context.
local function build_system()
    return "You are a coding assistant working in: " .. orcs.pwd .. "\n\n"
        .. build_tool_section() .. "\n\n"
        .. "When you need to use a tool, output EXACTLY this format on its own line:\n"
        .. 'TOOL_CALL: {"tool":"<name>","args":{<named_args>}}\n\n'
        .. 'Examples:\n'
        .. 'TOOL_CALL: {"tool":"read","args":{"path":"src/main.rs"}}\n'
        .. 'TOOL_CALL: {"tool":"grep","args":{"pattern":"TODO","path":"src"}}\n'
        .. 'TOOL_CALL: {"tool":"write","args":{"path":"out.txt","content":"hello"}}\n'
        .. 'TOOL_CALL: {"tool":"exec","args":{"cmd":"cargo test"}}\n\n'
        .. "You may output multiple TOOL_CALL lines in a single response.\n"
        .. "After receiving tool results, continue your analysis.\n"
        .. "When done, output your final answer without any TOOL_CALL lines."
end

--- Format a dispatch result into a human-readable string for LLM context.
local function format_result(tool_name, result)
    if not result.ok then
        return "[" .. tool_name .. "] ERROR: " .. (result.error or "unknown error")
    end

    if tool_name == "read" then
        local content = result.content or ""
        if #content > 2000 then
            content = content:sub(1, 2000) .. "\n... (truncated)"
        end
        return "[read] " .. (result.size or 0) .. " bytes:\n" .. content

    elseif tool_name == "grep" then
        local lines = {}
        local count = result.count or 0
        local show = math.min(count, 20)
        if result.matches then
            for i = 1, show do
                local m = result.matches[i]
                if m then
                    table.insert(lines, m.line_number .. ": " .. m.line)
                end
            end
        end
        local suffix = count > show and ("\n... (" .. count .. " total)") or ""
        return "[grep] " .. count .. " matches:\n" .. table.concat(lines, "\n") .. suffix

    elseif tool_name == "glob" then
        local items = {}
        local count = result.count or 0
        local show = math.min(count, 50)
        if result.files then
            for i = 1, show do
                if result.files[i] then
                    table.insert(items, result.files[i])
                end
            end
        end
        local suffix = count > show and ("\n... (" .. count .. " total)") or ""
        return "[glob] " .. count .. " files:\n" .. table.concat(items, "\n") .. suffix

    elseif tool_name == "exec" then
        local stdout = result.stdout or ""
        if #stdout > 2000 then
            stdout = stdout:sub(1, 2000) .. "\n... (truncated)"
        end
        return "[exec] OK:\n" .. stdout

    elseif tool_name == "write" then
        return "[write] OK: " .. (result.bytes_written or 0) .. " bytes"

    else
        return "[" .. tool_name .. "] OK"
    end
end

--- Parse TOOL_CALL lines from LLM response.
--- Returns: list of {tool, args}, remaining text
local function parse_tool_calls(text)
    local calls = {}
    local clean_lines = {}

    for line in text:gmatch("[^\n]+") do
        local json_str = line:match("^TOOL_CALL:%s*(.+)%s*$")
        if json_str then
            local ok, parsed = pcall(orcs.json_parse, json_str)
            if ok and type(parsed) == "table" and parsed.tool then
                table.insert(calls, {
                    tool = parsed.tool,
                    args = parsed.args or {},
                })
            else
                -- Malformed JSON — keep as text so the LLM sees it
                table.insert(clean_lines, line)
                orcs.log("warn", "[code_agent] malformed tool call: " .. json_str)
            end
        else
            table.insert(clean_lines, line)
        end
    end

    return calls, table.concat(clean_lines, "\n")
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

            -- Execute tool calls via dispatch
            local tool_results = {}
            for _, call in ipairs(calls) do
                orcs.log("info", "[code_agent] dispatch: " .. call.tool)
                local result = orcs.dispatch(call.tool, call.args)
                table.insert(tool_results, format_result(call.tool, result))
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

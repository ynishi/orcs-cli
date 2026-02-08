-- claude_cli.lua
-- Component that forwards user input to Claude CLI and returns the response.
--
-- Usage: Receives user message via on_request, spawns `claude` CLI,
-- and returns Claude's response.

return {
    id = "claude_cli",
    namespace = "builtin",
    subscriptions = {"UserInput"},
    output_to_io = true,
    elevated = true,

    on_request = function(request)
        -- Extract user message from payload
        local message = request.payload
        if type(message) == "table" then
            message = message.message or message.content or ""
        end
        if type(message) ~= "string" or message == "" then
            return { success = false, error = "empty message" }
        end

        orcs.log("info", "Calling Claude CLI with: " .. message:sub(1, 50) .. "...")

        -- Escape the message for shell
        local escaped = message:gsub("'", "'\\''")
        local cmd = "claude -p '" .. escaped .. "' 2>&1"

        local result = orcs.exec(cmd)

        if result.ok then
            -- Send response to ClientRunner via Output event
            orcs.output(result.stdout)
            return {
                success = true,
                data = {
                    response = result.stdout,
                    source = "claude_cli"
                }
            }
        else
            -- Send error to ClientRunner via Output event
            orcs.output("Claude CLI failed: " .. result.stderr, "error")
            return {
                success = false,
                error = "Claude CLI failed: " .. result.stderr,
                data = {
                    stdout = result.stdout,
                    stderr = result.stderr,
                    code = result.code
                }
            }
        end
    end,

    on_signal = function(signal)
        if signal.kind == "Veto" then
            return "Abort"
        end
        return "Ignored"
    end,

    init = function()
        orcs.log("info", "claude_cli component initialized")
    end,

    shutdown = function()
        orcs.log("info", "claude_cli component shutdown")
    end,
}

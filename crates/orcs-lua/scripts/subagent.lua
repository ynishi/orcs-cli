-- subagent.lua
-- Simple SubAgent component that echoes user input with a greeting.
--
-- Usage: Receives user message via UserInput broadcast and responds
-- with a formatted greeting.

return {
    id = "subagent",
    namespace = "builtin",
    subscriptions = {"UserInput"},

    on_request = function(request)
        -- Extract user message from payload
        local message = request.payload
        if type(message) == "table" then
            message = message.message or message.content or ""
        end
        if type(message) ~= "string" or message == "" then
            return { success = false, error = "empty message" }
        end

        orcs.log("info", "SubAgent received: " .. message:sub(1, 50))

        -- Format response
        local response = "Hello: " .. message

        -- Send response to ClientRunner via Output event
        orcs.output(response)

        return {
            success = true,
            data = {
                response = response,
                source = "subagent"
            }
        }
    end,

    on_signal = function(signal)
        if signal.kind == "Veto" then
            return "Abort"
        end
        return "Ignored"
    end,

    init = function()
        orcs.log("info", "subagent component initialized")
    end,

    shutdown = function()
        orcs.log("info", "subagent component shutdown")
    end,
}

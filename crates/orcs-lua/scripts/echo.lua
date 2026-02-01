-- echo.lua
-- Simple echo component for testing
return {
    id = "echo",
    namespace = "builtin",
    subscriptions = {"Echo"},

    on_request = function(request)
        if request.operation == "echo" then
            return { success = true, data = request.payload }
        end
        return { success = false, error = "unknown operation: " .. request.operation }
    end,

    on_signal = function(signal)
        if signal.kind == "Veto" then
            return "Abort"
        end
        return "Ignored"
    end,

    init = function()
        -- Optional initialization
    end,

    shutdown = function()
        -- Optional cleanup
    end,
}

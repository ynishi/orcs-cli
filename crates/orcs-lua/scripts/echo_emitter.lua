-- echo_emitter.lua
-- Echo component for ChannelRunner mode (uses orcs.output for output).
--
-- This script demonstrates ChannelRunner-based execution where output
-- is emitted via events instead of directly to IO.
--
-- Usage:
--   let component = ScriptLoader::load_embedded("echo_emitter")?;
--   engine.spawn_runner_full_auth(channel_id, Box::new(component), ...);
return {
    id = "echo_emitter",
    namespace = "builtin",
    subscriptions = {"Echo"},

    on_request = function(request)
        if request.operation == "echo" then
            -- Extract message from payload
            local message = request.payload
            if type(message) == "table" then
                message = message.message or message.content or ""
            end
            if type(message) ~= "string" then
                message = tostring(message)
            end

            -- Emit output via orcs.output (requires emitter to be set)
            orcs.output("Echo: " .. message)

            -- Return success
            return { success = true, data = { echoed = message } }
        end

        return { success = false, error = "unknown operation: " .. request.operation }
    end,

    on_signal = function(signal)
        if signal.kind == "Veto" then
            orcs.output_with_level("Received veto, aborting...", "warn")
            return "Abort"
        end
        return "Ignored"
    end,

    init = function()
        orcs.log("info", "echo_emitter component initialized")
    end,

    shutdown = function()
        orcs.log("info", "echo_emitter component shutdown")
    end,
}

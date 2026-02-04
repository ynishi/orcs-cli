-- shell.lua
-- Permission-checking shell component for auth verification.
--
-- Receives commands via @shell routing and executes them through
-- the permission checker (orcs.exec). Reports OK/NG results.
--
-- Usage (via @component routing):
--   @shell ls              → [shell] OK: <output>
--   @shell echo hello      → [shell] OK: hello
--   @shell sudo whoami      → [shell] NG: permission denied (elevation required)

return {
    id = "shell",
    namespace = "builtin",
    subscriptions = {"UserInput"},

    on_request = function(request)
        local cmd = request.payload
        if type(cmd) == "table" then
            cmd = cmd.message or cmd.content or ""
        end
        if type(cmd) ~= "string" or cmd == "" then
            orcs.output("[shell] Usage: @shell <command>")
            return { success = true }
        end

        -- Trim whitespace
        cmd = cmd:match("^%s*(.-)%s*$")
        if cmd == "" then
            orcs.output("[shell] Usage: @shell <command>")
            return { success = true }
        end

        orcs.log("info", "[shell] executing: " .. cmd)
        orcs.output("[shell] > " .. cmd)

        local result = orcs.exec(cmd)

        if result.ok then
            local stdout = result.stdout or ""
            stdout = stdout:gsub("%s+$", "")
            if stdout == "" then
                orcs.output("[shell] OK (no output)")
            else
                orcs.output("[shell] OK:\n" .. stdout)
            end
        else
            local stderr = result.stderr or "unknown error"
            stderr = stderr:gsub("%s+$", "")
            orcs.output("[shell] NG: " .. stderr)
        end

        return {
            success = result.ok,
            data = {
                cmd = cmd,
                ok = result.ok,
                stdout = result.stdout,
                stderr = result.stderr,
                code = result.code,
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
        orcs.log("info", "shell component initialized")
        orcs.output("[shell] Ready. Use @shell <command> to execute.")
    end,

    shutdown = function()
        orcs.log("info", "shell component shutdown")
    end,
}

-- shell.lua
-- Permission-checking shell component with HIL approval support.
--
-- Receives commands via @shell routing and executes them through
-- the permission checker (orcs.exec). Reports OK/NG results.
--
-- Supports three permission states:
--   allowed          → execute immediately
--   denied           → block with reason
--   requires_approval → prompt user, execute on approval
--
-- Usage (via @component routing):
--   @shell ls              → [shell] OK: <output>
--   @shell echo hello      → [shell] OK: hello
--   @shell sudo whoami     → [shell] NG: permission denied

-- Pending approval state: approval_id → { cmd, grant_pattern }
local pending = {}

--- Execute a command and report result.
local function execute_cmd(cmd)
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

    return result
end

return {
    id = "shell",
    namespace = "builtin",
    subscriptions = {"Echo"},
    output_to_io = true,
    elevated = false,

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

        -- Check permission before executing
        local check = orcs.check_command(cmd)

        if check.status == "allowed" then
            -- Execute immediately
            local result = execute_cmd(cmd)
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

        elseif check.status == "denied" then
            local reason = check.reason or "blocked by policy"
            orcs.output("[shell] NG: " .. reason)
            return { success = false, data = { cmd = cmd, denied = true, reason = reason } }

        elseif check.status == "requires_approval" then
            -- Submit approval request and wait for signal
            local desc = check.description or ("Execute: " .. cmd)
            local approval_id = orcs.request_approval("exec", desc)
            pending[approval_id] = {
                cmd = cmd,
                grant_pattern = check.grant_pattern,
            }
            orcs.output("[shell] Awaiting approval for: " .. cmd)
            return { success = true, data = { cmd = cmd, pending = true, approval_id = approval_id } }

        else
            orcs.output("[shell] NG: unknown permission status: " .. tostring(check.status))
            return { success = false }
        end
    end,

    on_signal = function(signal)
        if signal.kind == "Veto" then
            -- Clear all pending on veto
            pending = {}
            return "Abort"
        end

        if signal.kind == "Approve" and signal.approval_id then
            local info = pending[signal.approval_id]
            if info then
                pending[signal.approval_id] = nil
                orcs.log("info", "[shell] Approved: " .. info.cmd)

                -- Grant the pattern for future use
                if info.grant_pattern then
                    orcs.grant_command(info.grant_pattern)
                end

                -- Execute the previously pending command
                orcs.output("[shell] Approved. Executing: " .. info.cmd)
                execute_cmd(info.cmd)
                return "Handled"
            end
        end

        if signal.kind == "Reject" and signal.approval_id then
            local info = pending[signal.approval_id]
            if info then
                pending[signal.approval_id] = nil
                local reason = signal.reason or "rejected by user"
                orcs.output("[shell] Rejected: " .. info.cmd .. " (" .. reason .. ")")
                return "Handled"
            end
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

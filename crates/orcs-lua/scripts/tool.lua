-- tool.lua
-- File tool verification component.
--
-- Exercises orcs.read/write/grep/glob/mkdir/remove/mv through
-- the Capability-gated permission layer.
--
-- Usage (via @component routing):
--   @tool read <path>
--   @tool write <path> <content>
--   @tool grep <pattern> <path>
--   @tool glob <pattern> [dir]
--   @tool mkdir <path>
--   @tool remove <path>
--   @tool mv <src> <dst>
--   @tool pwd

--- Split string by first space(s).
local function split_first(s)
    local head, rest = s:match("^(%S+)%s+(.+)$")
    if head then
        return head, rest
    end
    return s, nil
end

--- Format a result table for output.
local function report(label, r)
    if r.ok then
        orcs.output("[tool] " .. label .. " OK")
    else
        orcs.output("[tool] " .. label .. " NG: " .. (r.error or "unknown error"))
    end
end

return {
    id = "tool",
    namespace = "builtin",
    subscriptions = {"Echo"},

    on_request = function(request)
        local msg = request.payload
        if type(msg) == "table" then
            msg = msg.message or msg.content or ""
        end
        if type(msg) ~= "string" or msg == "" then
            orcs.output("[tool] Usage: @tool <cmd> [args]")
            orcs.output("[tool] Commands: read, write, grep, glob, mkdir, remove, mv, pwd")
            return { success = true }
        end

        msg = msg:match("^%s*(.-)%s*$")
        local cmd, args = split_first(msg)
        cmd = cmd:lower()

        orcs.log("info", "[tool] cmd=" .. cmd .. " args=" .. tostring(args))

        if cmd == "pwd" then
            orcs.output("[tool] pwd: " .. orcs.pwd)
            return { success = true }

        elseif cmd == "read" then
            if not args then
                orcs.output("[tool] Usage: @tool read <path>")
                return { success = true }
            end
            local r = orcs.read(args)
            if r.ok then
                local preview = r.content
                if #preview > 200 then
                    preview = preview:sub(1, 200) .. "... (" .. r.size .. " bytes)"
                end
                orcs.output("[tool] read OK (" .. r.size .. " bytes):\n" .. preview)
            else
                orcs.output("[tool] read NG: " .. (r.error or "unknown"))
            end
            return { success = r.ok, data = { ok = r.ok, size = r.size, error = r.error } }

        elseif cmd == "write" then
            if not args then
                orcs.output("[tool] Usage: @tool write <path> <content>")
                return { success = true }
            end
            local path, content = split_first(args)
            if not content then
                content = ""
            end
            local r = orcs.write(path, content)
            report("write", r)
            return { success = r.ok, data = { ok = r.ok, bytes = r.bytes_written, error = r.error } }

        elseif cmd == "grep" then
            if not args then
                orcs.output("[tool] Usage: @tool grep <pattern> <path>")
                return { success = true }
            end
            local pattern, path = split_first(args)
            if not path then
                orcs.output("[tool] Usage: @tool grep <pattern> <path>")
                return { success = true }
            end
            local r = orcs.grep(pattern, path)
            if r.ok then
                orcs.output("[tool] grep OK: " .. r.count .. " matches")
                for i = 1, math.min(r.count, 10) do
                    local m = r.matches[i]
                    orcs.output("  " .. m.line_number .. ": " .. m.line)
                end
                if r.count > 10 then
                    orcs.output("  ... and " .. (r.count - 10) .. " more")
                end
            else
                orcs.output("[tool] grep NG: " .. (r.error or "unknown"))
            end
            return { success = r.ok, data = { ok = r.ok, count = r.count, error = r.error } }

        elseif cmd == "glob" then
            if not args then
                orcs.output("[tool] Usage: @tool glob <pattern> [dir]")
                return { success = true }
            end
            local pattern, dir = split_first(args)
            local r = orcs.glob(pattern, dir)
            if r.ok then
                orcs.output("[tool] glob OK: " .. r.count .. " files")
                for i = 1, math.min(r.count, 20) do
                    orcs.output("  " .. r.files[i])
                end
                if r.count > 20 then
                    orcs.output("  ... and " .. (r.count - 20) .. " more")
                end
            else
                orcs.output("[tool] glob NG: " .. (r.error or "unknown"))
            end
            return { success = r.ok, data = { ok = r.ok, count = r.count, error = r.error } }

        elseif cmd == "mkdir" then
            if not args then
                orcs.output("[tool] Usage: @tool mkdir <path>")
                return { success = true }
            end
            local r = orcs.mkdir(args)
            report("mkdir", r)
            return { success = r.ok }

        elseif cmd == "remove" then
            if not args then
                orcs.output("[tool] Usage: @tool remove <path>")
                return { success = true }
            end
            local r = orcs.remove(args)
            report("remove", r)
            return { success = r.ok }

        elseif cmd == "mv" then
            if not args then
                orcs.output("[tool] Usage: @tool mv <src> <dst>")
                return { success = true }
            end
            local src, dst = split_first(args)
            if not dst then
                orcs.output("[tool] Usage: @tool mv <src> <dst>")
                return { success = true }
            end
            local r = orcs.mv(src, dst)
            report("mv", r)
            return { success = r.ok }

        else
            orcs.output("[tool] Unknown command: " .. cmd)
            orcs.output("[tool] Commands: read, write, grep, glob, mkdir, remove, mv, pwd")
            return { success = false }
        end
    end,

    on_signal = function(signal)
        if signal.kind == "Veto" then
            return "Abort"
        end
        return "Ignored"
    end,

    init = function()
        orcs.log("info", "tool component initialized")
        orcs.output("[tool] Ready. Use @tool <cmd> [args] to exercise file tools.")
    end,

    shutdown = function()
        orcs.log("info", "tool component shutdown")
    end,
}

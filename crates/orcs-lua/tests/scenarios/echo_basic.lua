-- Echo Component: Basic Scenario Tests
--
-- Tests the fundamental request/signal cycle of a simple echo component.

local test = orcs.test

return {
    name = "Echo Component Basics",

    component = [[
        local state = { request_count = 0 }

        return {
            id = "echo",
            subscriptions = {"Echo"},

            on_request = function(req)
                state.request_count = state.request_count + 1

                if req.operation == "echo" then
                    return {
                        success = true,
                        data = {
                            message = req.payload.message,
                            count = state.request_count,
                        }
                    }
                elseif req.operation == "count" then
                    return {
                        success = true,
                        data = { count = state.request_count }
                    }
                end
                return { success = false, error = "unknown operation: " .. req.operation }
            end,

            on_signal = function(sig)
                if sig.kind == "Veto" then return "Abort" end
                if sig.kind == "Cancel" then return "Handled" end
                return "Ignored"
            end,
        }
    ]],

    scenarios = {
        -- =================================================================
        -- Request handling
        -- =================================================================
        {
            name = "echo returns payload message",
            run = function(h)
                local result = h:request("Echo", "echo", { message = "hello world" })
                test.eq(result.message, "hello world")
            end,
        },
        {
            name = "echo includes request count",
            run = function(h)
                local r1 = h:request("Echo", "echo", { message = "first" })
                test.eq(r1.count, 1)
            end,
        },
        {
            name = "count operation tracks requests",
            run = function(h)
                h:request("Echo", "echo", { message = "a" })
                h:request("Echo", "echo", { message = "b" })
                local count_result = h:request("Echo", "count", {})
                test.eq(count_result.count, 3) -- echo + echo + count itself
            end,
        },
        {
            name = "unknown operation returns error",
            run = function(h)
                local ok, err = pcall(function()
                    h:request("Echo", "nonexistent", {})
                end)
                test.ok(not ok, "should return error for unknown op")
                test.contains(tostring(err), "unknown operation")
            end,
        },

        -- =================================================================
        -- Signal handling
        -- =================================================================
        {
            name = "veto signal aborts component",
            run = function(h)
                test.eq(h:status(), "Idle")
                local response = h:veto()
                test.eq(response, "Abort")
                test.eq(h:status(), "Aborted")
            end,
        },
        {
            name = "cancel signal is handled",
            run = function(h)
                local response = h:cancel()
                test.eq(response, "Handled")
                test.eq(h:status(), "Idle") -- should stay Idle
            end,
        },

        -- =================================================================
        -- Harness isolation
        -- =================================================================
        {
            name = "each scenario gets fresh state",
            run = function(h)
                -- If state leaked from previous scenarios, count would be > 1
                local result = h:request("Echo", "echo", { message = "fresh" })
                test.eq(result.count, 1, "should be first request in fresh harness")
            end,
        },

        -- =================================================================
        -- Logging
        -- =================================================================
        {
            name = "request log captures operations",
            run = function(h)
                h:request("Echo", "echo", { message = "a" })
                h:request("Echo", "count", {})

                local log = h:request_log()
                test.len(log, 2)
                test.eq(log[1].operation, "echo")
                test.eq(log[1].success, true)
                test.eq(log[2].operation, "count")
            end,
        },
        {
            name = "signal log captures responses",
            run = function(h)
                h:cancel()
                h:veto()

                local log = h:signal_log()
                test.len(log, 2)
                test.eq(log[1].response, "Handled")
                test.eq(log[2].response, "Abort")
            end,
        },
        {
            name = "clear_logs resets history",
            run = function(h)
                h:request("Echo", "echo", { message = "x" })
                h:cancel()
                h:clear_logs()

                test.len(h:request_log(), 0)
                test.len(h:signal_log(), 0)
            end,
        },
    },
}

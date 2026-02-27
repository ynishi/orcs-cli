-- Echo Component: Basic Scenario Tests
--
-- Tests the fundamental request/signal cycle of a simple echo component.

local expect = lust.expect

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
                expect(result.message).to.equal("hello world")
            end,
        },
        {
            name = "echo includes request count",
            run = function(h)
                local r1 = h:request("Echo", "echo", { message = "first" })
                expect(r1.count).to.equal(1)
            end,
        },
        {
            name = "count operation tracks requests",
            run = function(h)
                h:request("Echo", "echo", { message = "a" })
                h:request("Echo", "echo", { message = "b" })
                local count_result = h:request("Echo", "count", {})
                expect(count_result.count).to.equal(3) -- echo + echo + count itself
            end,
        },
        {
            name = "unknown operation returns error",
            run = function(h)
                local ok, err = pcall(function()
                    h:request("Echo", "nonexistent", {})
                end)
                expect(ok).to.equal(false)
                expect(tostring(err)).to.match("unknown operation")
            end,
        },

        -- =================================================================
        -- Signal handling
        -- =================================================================
        {
            name = "veto signal aborts component",
            run = function(h)
                expect(h:status()).to.equal("Idle")
                local response = h:veto()
                expect(response).to.equal("Abort")
                expect(h:status()).to.equal("Aborted")
            end,
        },
        {
            name = "cancel signal is handled",
            run = function(h)
                local response = h:cancel()
                expect(response).to.equal("Handled")
                expect(h:status()).to.equal("Idle") -- should stay Idle
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
                expect(result.count).to.equal(1)
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
                expect(#log).to.equal(2)
                expect(log[1].operation).to.equal("echo")
                expect(log[1].success).to.equal(true)
                expect(log[2].operation).to.equal("count")
            end,
        },
        {
            name = "signal log captures responses",
            run = function(h)
                h:cancel()
                h:veto()

                local log = h:signal_log()
                expect(#log).to.equal(2)
                expect(log[1].response).to.equal("Handled")
                expect(log[2].response).to.equal("Abort")
            end,
        },
        {
            name = "clear_logs resets history",
            run = function(h)
                h:request("Echo", "echo", { message = "x" })
                h:cancel()
                h:clear_logs()

                expect(#h:request_log()).to.equal(0)
                expect(#h:signal_log()).to.equal(0)
            end,
        },
    },
}

-- Echo Component: Scenario Tests
--
-- Tests the built-in echo component loaded via component_name.
-- This validates that named script loading works through the scenario framework.

local test = orcs.test

return {
    name = "Echo Component",

    -- Load the echo component from the crate's scripts/ directory
    component_name = "echo",

    scenarios = {
        {
            name = "embedded echo component loads",
            run = function(h)
                test.eq(h:status(), "Idle")
            end,
        },
        {
            name = "embedded echo handles request",
            run = function(h)
                -- The built-in echo component echoes payload on "echo" operation
                local result = h:request("Echo", "echo", { msg = "test" })
                test.type_is(result, "table", "should return a table")
            end,
        },
        {
            name = "embedded echo responds to veto",
            run = function(h)
                local response = h:veto()
                test.eq(response, "Abort")
                test.eq(h:status(), "Aborted")
            end,
        },
    },
}
